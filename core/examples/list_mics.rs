//! Prints the microphones PathoSync can see:  cargo run -p pathosync-core --example list_mics
fn main() {
    match pathosync_core::list_input_devices() {
        Ok(devs) => {
            for d in devs {
                println!("{}{}  [{} ch, {} Hz]  id={}", if d.is_default { "* " } else { "  " }, d.name, d.channels, d.sample_rate, d.id);
            }
        }
        Err(e) => eprintln!("{e}"),
    }
}
