# Build separately from offline execution; rustup may download components.
# No checkout build context is needed: docker build -t recalld-ci:1.94 - < this-file
FROM rust@sha256:6ae102bdbf528294bc79ad6e1fae682f6f7c2a6e6621506ba959f9685b308a55
RUN rustup component add rustfmt clippy --toolchain 1.94.1
ENV RUSTUP_TOOLCHAIN=1.94.1
