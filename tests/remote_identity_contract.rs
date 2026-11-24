use clatterforge::repository::remote_identity;

#[test]
fn scp_remotes_remove_query_and_fragment_credentials() {
    assert_eq!(
        remote_identity("git@Git.Example:team/project.git?access_token=secret#anchor").unwrap(),
        "ssh://git.example/team/project"
    );
    assert_eq!(
        remote_identity("git@Git.Example:team/project.git#anchor").unwrap(),
        "ssh://git.example/team/project"
    );
    assert_eq!(
        remote_identity("https://user:secret@Git.Example/team/project.git?token=secret#anchor")
            .unwrap(),
        "https://git.example/team/project"
    );
}

#[test]
fn invalid_scp_host_or_repository_is_rejected() {
    assert!(remote_identity("git@:team/project.git").is_err());
    assert!(remote_identity("git@Git.Example:").is_err());
    assert!(remote_identity("git@Git.Example:?token=secret").is_err());
}

#[test]
fn existing_local_and_remote_identity_behavior_is_preserved() {
    assert_eq!(
        remote_identity("git@Git.Example:team/project.git").unwrap(),
        "ssh://git.example/team/project"
    );
    assert_eq!(
        remote_identity(r"C:\repos\project#draft").unwrap(),
        "file:C:/repos/project#draft"
    );
    assert_eq!(
        remote_identity("/srv/project#draft").unwrap(),
        "file:/srv/project#draft"
    );
    assert_eq!(
        remote_identity("file:///srv/project#draft").unwrap(),
        "file:///srv/project"
    );
}
