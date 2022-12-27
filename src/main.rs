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

fn get_redis_connection(url: String) -> Connection {
    let client = Client::open(url).unwrap();
    client.get_connection().unwrap()
}

fn get_shortlink(mut con: &mut Connection, keyword: &str) -> Result<Shortlink, redis::RedisError> {
    let serialized: String = redis::cmd("GET").arg(keyword).query(&mut con)?;
    let shortlink: Shortlink = serde_json::from_str(&serialized).unwrap();
    Ok(shortlink)
}

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

fn store_shortlink(mut con: &mut Connection,
                   shortlink: &Shortlink)
                   -> Result<(), redis::RedisError> {
    let shortlink_string = serde_json::to_string(shortlink).unwrap();
    redis::cmd("SET").arg(&shortlink.keyword)
                     .arg(&shortlink_string)
                     .execute(&mut con);
    Ok(())
}

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

// Define a vector to contain a list of shortlinks
type Shortlinks = Vec<Shortlink>;

// A simple struct to represent a shortlink
#[derive(Default, Serialize, Deserialize)]
struct Shortlink {
    // #[serde(skip_serializing_if = "Option::is_none")]
    id: String,
    keyword: String,
    url: String,
    // #[serde(skip_serializing_if = "Option::is_none")]
    hits: usize,
    // #[serde(skip_serializing_if = "Option::is_none")]
    private: bool,
    // #[serde(skip_serializing_if = "Option::is_none")]
    owner: String,
    // #[serde(skip_serializing_if = "Option::is_none")]
    description: String,
}

// Implement a method to generate a unique short uuid
impl Shortlink {
    fn generate_id(&mut self) {
        self.id = uuid::Uuid::new_v4().to_string();
    }
}

// Implement the Debug trait for the Shortlink struct
impl std::fmt::Debug for Shortlink {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Shortlink {{ id: {:?}, keyword: {}, url: {}, hits: {:?}, description: {:?}, private: {:?}, owner: {:?} }}", self.id, self.keyword, self.url, self.hits, self.description, self.private, self.owner)
    }
}

// Write a function to implement the hit counter in redis of a link
fn hit(con: &mut Connection, keyword: &str) {
    let mut shortlink = get_shortlink(con, keyword).unwrap();
    shortlink.hits += 1;
    store_shortlink(con, &shortlink).unwrap();
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
    let mut con = get_redis_connection(REDIS_URL.to_string());
    let shortlink = get_shortlink(&mut con, &keyword).unwrap_or(default_shortlink(keyword));
    let mut context = context! {
        create: true,
        shortlink: &shortlink,
    };
    if keyword == shortlink.keyword {
        context.create = false;
    };
    println!("{sholrtlink:?}");
    Template::render("edit", &context)
}

fn default_shortlink(keyword: String) -> Shortlink {
    Shortlink {
        id: keyword,
        keyword: String::from(""),
        url: String::from(""),
        hits: 0,
        private: false,
        owner: String::from(""),
        description: String::from(""),
    }
}

#[post("/<keyword>",
       format = "application/x-www-form-urlencoded",
       data = "<body>",
       rank = 2)]
fn save_keyword(keyword: String, body: &str) -> Template {
    let mut con = get_redis_connection(REDIS_URL.to_string());
    // Serialize the body to a struct
    let body: Shortlink = serde_urlencoded::from_str(body).unwrap_or(default_shortlink(keyword));
    // Check if keyword exists and if so update the modified fields
    let mut shortlink = get_shortlink(&mut con, &keyword).unwrap();
    if keyword == shortlink.keyword {
        shortlink.url = body.url;
        shortlink.private = body.private;
        shortlink.description = body.description;
        shortlink.owner = body.owner;
    } else {
        shortlink.generate_id();
        shortlink.keyword = body.keyword;
        shortlink.url = body.url;
        shortlink.private = body.private;
        shortlink.description = body.description;
    }
    store_shortlink(&mut con, &shortlink).unwrap();
    let ctx = context! {
        saved: "true",
        keyword: shortlink.keyword,
        url: shortlink.url,
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

#[get("/<keyword>", rank = 1)]
fn get_keyword(keyword: String) -> (Status, Redirect) {
    format!("Looking to get keyword {keyword}");
    // Get the keyword from the database or redirect to the edit page
    let mut con = get_redis_connection(REDIS_URL.to_string());
    let shortlink = get_shortlink(&mut con, &keyword);
    if let Ok(shortlink) = shortlink {
        hit(&mut con, &keyword);
        let url = shortlink.url;
        let redirect = Redirect::temporary(url);
        (Status::TemporaryRedirect, redirect)
    } else {
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
                          routes![get_keywords, get_keyword, edit_keyword, save_keyword])
}
