//! Trae CLI (`traecli`): generic ACP; slash commands and skills arrive asynchronously.

use std::time::Duration;

use super::generic::{generic_client, GenericOptions};
use crate::client::AcpClient;

const INITIAL_COMMANDS_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

pub fn options(command: Vec<String>) -> GenericOptions {
    let mut o = GenericOptions::new(command);
    o.wait_for_initial_commands = true;
    o.initial_commands_wait_timeout = INITIAL_COMMANDS_WAIT_TIMEOUT;
    o
}

pub fn client(command: Vec<String>) -> AcpClient {
    generic_client(options(command))
}
