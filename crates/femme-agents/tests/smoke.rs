mod common;

#[test]
fn shell_driver_is_inert() {
    let mut d = femme_agents::driver_for(femme_proto::state::Agent::Shell);
    let mut out = femme_agents::Out::default();
    assert!(d
        .prompt("x", femme_proto::rpc::PromptHow::Queue, &mut out)
        .is_err());
    assert!(out.stdin.is_empty());
}
