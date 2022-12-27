# Link Shrink

## Dependencies

Install clippy for warnings.

Cargo watch for autobuild on code changes.

Rocket requires dev or nightly toolchain.

Also needs redis, just launch the docker-compose file with `docker-compose up -d`.

## Running 

```shell
$ RUST_BACKTRACE=1 cargo watch -x run -x clippy
```
