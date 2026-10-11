# A small, generic image: one static binary in an empty image, running as a non-root user, links in /data.
#   docker build -t linkshrink .
#   docker run -p 8080:8080 -v linkshrink:/data -v ./gate-secret:/run/secrets/gate:ro \
#     -e LINKSHRINK_PUBLIC_ORIGIN=https://go.example.com -e LINKSHRINK_TRUSTED_PROXY=172.17.0.1 \
#     -e LINKSHRINK_GATE_SECRET_FILE=/run/secrets/gate linkshrink
# It must sit behind a sign-in proxy (see examples/): it has no login of its own and refuses every request without one.
FROM docker.io/library/rust:1.98-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN cargo test --locked --quiet && cargo build --locked --release --quiet \
 && mkdir -p /out/data /out/tmp && cp target/release/linkshrink /out/linkshrink

FROM scratch
COPY --from=build /out/linkshrink /linkshrink
COPY --from=build --chown=65532:65532 /out/data /data
COPY --from=build --chown=65532:65532 /out/tmp /tmp
ENV LINKSHRINK_LISTEN=0.0.0.0:8080 LINKSHRINK_DB=/data/links.db
USER 65532:65532
EXPOSE 8080
VOLUME /data
HEALTHCHECK CMD ["/linkshrink", "healthcheck"]
ENTRYPOINT ["/linkshrink"]
