FROM rust:1.88-bookworm AS builder

RUN apt-get update \
    && apt-get install --yes --no-install-recommends python3 python3-venv \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

RUN python3 -m venv /venv \
    && /venv/bin/pip install --no-cache-dir 'maturin>=1.7,<2.0' \
    && /venv/bin/maturin build --release --locked \
       --manifest-path crates/ermya-graph-python/Cargo.toml \
       --out /dist

FROM builder AS test
RUN /venv/bin/pip install --no-cache-dir pytest /dist/*.whl \
    && /venv/bin/python -m pytest crates/ermya-graph-python/tests -q

FROM scratch AS wheel
COPY --from=builder /dist /

