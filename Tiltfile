# Trace Registry - Local Development with Tilt
# Run `tilt up` to start the dev loop.

local_resource(
    'opencargo',
    serve_cmd='cargo run -- --config config.toml',
    deps=['src', 'Cargo.toml'],
    auto_init=True,
    readiness_probe=probe(
        http_get=http_get_action(port=6789, path='/health/live'),
        period_secs=2,
    ),
)

local_resource(
    'opencargo-test',
    cmd='cargo test',
    deps=['src', 'Cargo.toml'],
    auto_init=False,
    trigger_mode=TRIGGER_MODE_MANUAL,
)
