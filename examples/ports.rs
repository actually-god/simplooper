use jack::{Client, ClientOptions, PortFlags};

fn main() {
    let (client, _status) = Client::new("hermes-probe", ClientOptions::NO_START_SERVER).unwrap();
    println!("== INPUTS (remote output ports) ==");
    for n in client.ports(None, Some("audio"), PortFlags::IS_OUTPUT) {
        if !n.starts_with("hermes-probe:") {
            println!("{n}");
        }
    }
    println!("== OUTPUTS (remote input ports) ==");
    for n in client.ports(None, Some("audio"), PortFlags::IS_INPUT) {
        if !n.starts_with("hermes-probe:") {
            println!("{n}");
        }
    }
}
