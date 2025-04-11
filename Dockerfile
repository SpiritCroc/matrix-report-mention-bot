FROM docker.io/rust:1.85.0

WORKDIR /usr/src/matrix-report-mention-bot
COPY . .

RUN cargo install --path .

CMD ["matrix-report-mention-bot"]
