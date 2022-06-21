// RGB node providing smart contracts functionality for Bitcoin & Lightning.
//
// Written in 2022 by
//     Dr. Maxim Orlovsky <orlovsky@lnp-bp.org>
//
// Copyright (C) 2022 by LNP/BP Standards Association, Switzerland.
//
// You should have received a copy of the MIT License along with this software.
// If not, see <https://opensource.org/licenses/MIT>.

use microservices::esb;
use microservices::shell::Exec;
use rgb_rpc::{Client, RpcMsg, ServiceId};

use crate::{Command, Opts};

impl Command {
    pub fn action_string(&self) -> String {
        match self {
            Command::Register { contract } => {
                format!("Registering contract {}", contract.contract_id())
            }
            Command::Contracts => s!("Listring contracts"),
            Command::State { contract_id } => format!("Quering state of {}", contract_id),
            Command::Contract { contract_id } => {
                format!("Retrieving contract source for {}", contract_id)
            }
        }
    }
}

impl Exec for Opts {
    type Client = Client;
    type Error = esb::Error<ServiceId>;

    fn exec(self, client: &mut Self::Client) -> Result<(), Self::Error> {
        println!("{}...", self.command.action_string());
        match self.command {
            Command::Register { contract } => {
                client.request(RpcMsg::AddContract(contract))?;
                client.report_progress()?;
            }
            Command::Contracts => {
                client.list_contracts()?.iter().for_each(|id| println!("{}", id));
            }
            Command::State { contract_id } => {
                let state = client.contract_state(contract_id)?;
                println!("{}", serde_yaml::to_string(&state).unwrap());
            }
            Command::Contract { contract_id } => {
                let contract = client.contract(contract_id)?;
                println!("{}", contract);
            }
        };

        Ok(())
    }
}
