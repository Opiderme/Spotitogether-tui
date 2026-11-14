# Serveur de salon Spotitogether (le client TUI s'utilise hors conteneur).
#   docker build -t spotitogether .
#   docker run -d --name salon -p 7420:7420 -v spotitogether:/data spotitogether --code SALON1
#   docker logs salon        # code d'invitation et jeton d'hôte

FROM rust:1-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p spotitogether

FROM debian:trixie-slim
RUN useradd --system --uid 10001 spotitogether \
    && mkdir /data && chown spotitogether /data
COPY --from=build /src/target/release/spotitogether /usr/local/bin/spotitogether
USER spotitogether
VOLUME /data
EXPOSE 7420
ENTRYPOINT ["spotitogether", "serve", "--bind", "0.0.0.0", "--data", "/data/room.sqlite"]
