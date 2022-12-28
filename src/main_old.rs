#[macro_use]
extern crate rocket;

use std::path::Path;

// Import the necessary crates
use redis::{Client, Connection};
use rocket::{fs::NamedFile,
             http::{ContentType, Status},
             response::Redirect};
use rocket_dyn_templates::{context, Template};
use serde::{Deserialize, Serialize};

// Global variable to store the redis connection url
const REDIS_URL: &str = "redis://localhost";

/// Get a redis database connection
fn get_redis_connection(url: String) -> Connection {
    let client = Client::open(url).unwrap();
    client.get_connection().unwrap()
}

/// get a shortlink
fn get_shortlink(mut con: &mut Connection, keyword: &str) -> Result<Shortlink, redis::RedisError> {
    let serialized: String = redis::cmd("GET").arg(keyword).query(&mut con)?;
    let shortlink: Shortlink = serde_json::from_str(&serialized).unwrap();
    Ok(shortlink)
}

/// get all shortlinks
fn get_all_shortlinks(mut con: &mut Connection) -> Result<Shortlinks, redis::RedisError> {
    let keys: Vec<String> = redis::cmd("KEYS").arg("*").query(&mut con)?;
    let mut shortlinks = Vec::new();
    for key in keys {
        let serialized: String = redis::cmd("GET").arg(key).query(&mut con)?;
        let shortlink: Shortlink = serde_json::from_str(&serialized).unwrap();
        shortlinks.push(shortlink);
    }
    Ok(shortlinks)
}

/// save a shortlink
fn store_shortlink(mut con: &mut Connection,
                   shortlink: &Shortlink)
                   -> Result<(), redis::RedisError> {
    let shortlink_string = serde_json::to_string(shortlink).unwrap();
    redis::cmd("SET").arg(&shortlink.keyword)
                     .arg(&shortlink_string)
                     .execute(&mut con);
    Ok(())
}

///save multiple shortlinks
fn store_shortlinks(mut con: &mut Connection,
                    shortlinks: &Shortlinks)
                    -> Result<(), redis::RedisError> {
    for shortlink in shortlinks {
        let shortlink_string = serde_json::to_string(shortlink).unwrap();
        redis::cmd("SET").arg(&shortlink.keyword)
                         .arg(&shortlink_string)
                         .execute(&mut con);
    }
    Ok(())
}

/// A vector to contain a list of shortlinks
type Shortlinks = Vec<Shortlink>;

/// A simple struct to represent a shortlink
#[derive(Default, Serialize, Deserialize, Debug)]
struct Shortlink {
    id: Option<String>,
    keyword: String,
    url: String,
    hits: Option<usize>,
    #[serde(default)]
    private: bool,
    owner: String,
    description: String,
}

/// Implements a method to generate a unique short uuid
impl Shortlink {
    fn generate_id(&mut self) {
        self.id = Some(uuid::Uuid::new_v4().to_string());
    }
}
/// Implement the Debug trait for the Shortlink struct
// #[derive(debug)]
// impl std::fmt::Debug for Shortlink {
//     fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
//         write!(f, "Shortlink {{ id: {:?}, keyword: {}, url: {}, hits: {:?}, description: {:?}, private: {:?}, owner: {:?} }}", self.id, self.keyword, self.url, self.hits, self.description, self.private, self.owner)
//     }
// }

/// Write a function to implement the hit counter in redis of a link
fn hit(con: &mut Connection, keyword: &str) {
    let mut shortlink = get_shortlink(con, keyword).unwrap();
    let hits = shortlink.hits.get_or_insert(0);
    *hits += 1;
    store_shortlink(con, &shortlink).unwrap();
}

fn default_shortlink(keyword: String) -> Shortlink {
    Shortlink { id: Some(uuid::Uuid::new_v4().to_string()),
                keyword,
                url: String::from(""),
                hits: Some(0),
                private: false,
                owner: String::from("admin"),
                description: String::from("") }
}

#[get("/")]
fn index() -> (Status, (ContentType, &'static str)) {
    let response = "Hello, world!";
    (Status::Ok, (ContentType::HTML, response))
}

// Return all links in the database at a nicely formatted HTML table using a template/
#[get("/links")]
fn get_keywords() -> Template {
    // List of keywords
    let mut con = get_redis_connection(REDIS_URL.to_string());
    let shortlinks = get_all_shortlinks(&mut con).unwrap();
    let context = context! {
        shortlinks: shortlinks,
    };
    Template::render("links", &context)
}

#[get("/edit/<keyword>")]
fn edit_keyword(keyword: String) -> Template {
    println!("Someone wants to edit the keyword: {keyword}");
    let mut con = get_redis_connection(REDIS_URL.to_string());
    let shortlink: Shortlink =
        get_shortlink(&mut con, &keyword).unwrap_or(default_shortlink(keyword));
    println!("Shortlink Object: {shortlink:#?}");
    let mut create = false;
    if shortlink.url.is_empty() {
        println!("Shortlink {} has an empty URL, so it's probably new...",
                 shortlink.keyword);
        create = true;
    };
    let context = context! {
        create: create,
        shortlink: &shortlink,
    };
    println!("{shortlink:#?}");
    Template::render("edit", &context)
}

#[post("/<keyword>",
       format = "application/x-www-form-urlencoded",
       data = "<body>",
       rank = 2)]
fn new_keyword(keyword: &str, body: &str) -> Template {
    println!("Someone wants to create shortlink {keyword}.");
    let mut con = get_redis_connection(REDIS_URL.to_string());
    // Serialize the body to a struct
    let mut shortlink: Shortlink = serde_urlencoded::from_str(body).unwrap();
    println!("Received deserialized body: {shortlink:#?}");
    shortlink.keyword = keyword.to_string();
    shortlink.generate_id();
    println!("Updated ID deserialized body: {shortlink:#?}");
    //     Some(p) if p == "on" => Some("true".to_string()),
    //     _ => Some("".to_string()),
    // };
    store_shortlink(&mut con, &shortlink).unwrap();
    let ctx = context! {
        saved: "true",
        shortlink,
    };
    Template::render("edit", &ctx)
}

#[post("/<keyword>/update",
       format = "application/x-www-form-urlencoded",
       data = "<body>",
       rank = 1)]
fn update_keyword(keyword: String, body: &str) -> Template {
    println!("Someone wants to update the keyword: {keyword}, with body: {body}");
    // Serialize the body to a struct
    let new_shortlink: Shortlink = serde_urlencoded::from_str(body).unwrap();
    // Get the existing shortlink from the database
    let mut con = get_redis_connection(REDIS_URL.to_string());
    let mut shortlink = get_shortlink(&mut con, &new_shortlink.keyword).unwrap();
    println!("Would like to update retrieved DB entry: {shortlink:#?}");
    println!("With {new_shortlink:#?}");

    // Update the old with the new values from the body
    shortlink.url = new_shortlink.url;
    shortlink.private = new_shortlink.private;
    //     Some(p) if p == "on" => true,
    //     _ => false,
    // };
    shortlink.owner = new_shortlink.owner;
    shortlink.description = new_shortlink.description;

    // Store the updated shortlink in the database
    let save = store_shortlink(&mut con, &shortlink);
    match save {
        Ok(_) => println!("Saved {shortlink:#?}"),
        Err(e) => println!("Error saving {shortlink:#?}: {e:?}"),
    }
    store_shortlink(&mut con, &shortlink).unwrap();
    let ctx = context! {
        saved: "true",
        shortlink,
    };
    Template::render("edit", &ctx)
}

// mod keyword {
//     #[post("/<keyword>")]
//     pub fn get_keyword(request: rocket::Request, keyword: String) {
//         match request.method() {
//             rocket::http::Method::Get => {
//                 println!("Get request")
//             },
//             rocket::http::Method::Post => {
//                 let body = request.to_string();
//                 println!("Body: {}", body)
//             },
//             _ => (),
//         }
//         format!("Looking to get keyword {keyword}");
//     }
// }

#[get("/<keyword>", rank = 3)]
fn get_keyword(keyword: String) -> (Status, Redirect) {
    println!("Looking to get keyword {keyword}");
    // Get the keyword from the database or redirect to the edit page
    let mut con = get_redis_connection(REDIS_URL.to_string());
    let shortlink = get_shortlink(&mut con, &keyword);
    if let Ok(shortlink) = shortlink {
        println!("Keyword {keyword} exists, 🚀 {:?} !", shortlink.url);
        hit(&mut con, &keyword);
        let url = shortlink.url;
        let redirect = Redirect::temporary(url);
        (Status::TemporaryRedirect, redirect)
    } else {
        println!("Doesn't exist, redirecting to /edit/{keyword} to create...");
        let redirect = Redirect::temporary(format!("/edit/{keyword}"));
        (Status::TemporaryRedirect, redirect)
    }
}

#[get("/favicon.ico")]
async fn favicon() -> Option<NamedFile> {
    NamedFile::open(Path::new("assets/img/favicon.ico")).await
                                                        .ok()
}

#[launch]
fn rocket() -> _ {
    rocket::build().attach(Template::fairing())
                   .mount("/", routes![favicon])
                   .mount("/", routes![index])
                   .mount("/",
                          routes![get_keywords,
                                  get_keyword,
                                  edit_keyword,
                                  new_keyword,
                                  update_keyword])
}
