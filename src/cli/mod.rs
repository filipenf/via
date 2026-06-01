mod agent;
mod session;

use anyhow::Result;

pub enum CliCommand {
    Session(session::SessionCli),
    Agent(agent::AgentCli),
}

pub fn parse_cli(args: &[String]) -> Result<Option<CliCommand>> {
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--review-backend" => {
                index += 2;
            }
            "--open" => {
                return Ok(None);
            }
            "session" => {
                index += 1;
                return Ok(Some(CliCommand::Session(session::SessionCli::parse(
                    &args[index..],
                )?)));
            }
            "agent" => {
                index += 1;
                return Ok(Some(CliCommand::Agent(agent::AgentCli::parse(
                    &args[index..],
                )?)));
            }
            _ => index += 1,
        }
    }
    Ok(None)
}

pub async fn run(command: CliCommand) -> Result<()> {
    match command {
        CliCommand::Session(command) => session::run(command).await,
        CliCommand::Agent(command) => agent::run(command),
    }
}
