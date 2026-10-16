use paseo_provider_host::{Error, HostConfig, ProviderHostClient};

#[tokio::test]
async fn host_that_dies_during_initialize_reports_exit_and_stderr() {
    let config = HostConfig::new("sh", ["-c", "echo boom >&2; exit 3"]);
    let error = match ProviderHostClient::spawn(config, None).await {
        Ok(_) => panic!("spawn should fail"),
        Err(error) => error,
    };
    match error {
        Error::HostExited { stderr_tail, .. } => {
            assert!(stderr_tail.contains("boom"), "{stderr_tail}")
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn missing_executable_is_a_spawn_error() {
    let config = HostConfig::new("/nonexistent/paseo-host-binary", Vec::<String>::new());
    assert!(matches!(
        ProviderHostClient::spawn(config, None).await,
        Err(Error::Spawn(_))
    ));
}

#[tokio::test]
async fn host_speaking_garbage_times_out_instead_of_hanging() {
    let mut config = HostConfig::new("sh", ["-c", "echo not-json; sleep 5"]);
    config.control_timeout = Some(std::time::Duration::from_millis(300));
    let error = match ProviderHostClient::spawn(config, None).await {
        Ok(_) => panic!("spawn should fail"),
        Err(error) => error,
    };
    assert!(matches!(error, Error::Timeout(_)), "{error:?}");
}
