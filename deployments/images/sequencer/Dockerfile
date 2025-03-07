# syntax = devthefuture/dockerfile-x

INCLUDE deployments/images/base/Dockerfile


# Compile the sequencer_node crate in release mode, ensuring dependencies are locked.
FROM base AS builder
WORKDIR /app
COPY . .
RUN cargo build --bin starknet_sequencer_node

FROM ubuntu:24.04 as final_stage

ENV ID=1001
WORKDIR /app
COPY --from=builder /app/target/debug/starknet_sequencer_node ./target/debug/starknet_sequencer_node
COPY --from=builder /usr/bin/tini /usr/bin/tini

# Copy sequencer config
COPY config/sequencer config/sequencer

# Create a new user "sequencer".
RUN set -ex; \
    groupadd --gid ${ID} sequencer; \
    useradd --gid ${ID} --uid ${ID} --comment "" --create-home --home-dir /app sequencer; \
    mkdir /data; \
    chown -R sequencer:sequencer /app /data

# Expose RPC and monitoring ports.
EXPOSE 8080 8081 8082

# Switch to the new user.
USER ${ID}

# Set the entrypoint to use tini to manage the process.
ENTRYPOINT ["tini", "--", "/app/target/debug/starknet_sequencer_node"]
