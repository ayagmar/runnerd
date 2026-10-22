use super::{JitConfig, RunnerSpec, create_body};
use crate::isolation::Sysbox;

const SECRET: &str = "sentinel-jit-config-c2VjcmV0";

#[test]
fn the_jit_config_stays_out_of_debug_output_and_the_create_request() {
    let spec = RunnerSpec {
        name: "runnerd-trellis-1".to_owned(),
        image: "runnerd-runner:trellis".to_owned(),
        profile: "trellis".to_owned(),
        slice: "ci.slice".to_owned(),
        cpus: 8,
        memory: "9G".parse().unwrap(),
        env: Vec::new(),
        mounts: Vec::new(),
        entrypoint: None,
        jit_config: Some(JitConfig(SECRET.to_owned())),
    };
    assert!(!format!("{spec:?}").contains(SECRET));
    let body = create_body(&spec, &Sysbox);
    assert!(!format!("{body:?}").contains(SECRET));
    assert!(!serde_json::to_string(&body).unwrap().contains(SECRET));
}
