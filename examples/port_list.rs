//! List all JACK audio ports as seen by the jack crate (same view slooper gets).
use jack::{Client, ClientOptions, PortFlags};

fn main() {
    let (client, _status) = match Client::new("port_list", ClientOptions::NO_START_SERVER) {
        Ok(c) => c,
        Err(e) => { eprintln!("no JACK server: {e:?}"); std::process::exit(1); }
    };
    println!("== remote OUTPUT ports (app's input candidates) ==");
    for n in client.ports(None, Some("audio"), PortFlags::IS_OUTPUT) { println!("{n}"); }
    println!("== remote INPUT ports (app's output candidates) ==");
    for n in client.ports(None, Some("audio"), PortFlags::IS_INPUT) { println!("{n}"); }
}
