pub(super) fn run_omp_command(args: &[String]) -> std::io::Result<i32> {
    let Some(action) = args.first().map(String::as_str) else {
        print_omp_help();
        return Ok(2);
    };
    match action {
        "guest-bridge" if args.len() == 4 => {
            let route_generation = match args[3].parse() {
                Ok(generation) => generation,
                Err(_) => {
                    eprintln!("route generation must be an unsigned integer");
                    return Ok(2);
                }
            };
            crate::client::run_omp_guest_bridge(
                args[1].clone(),
                args[2].clone(),
                route_generation,
            )?;
            Ok(0)
        }
        "help" | "--help" | "-h" => {
            print_omp_help();
            Ok(0)
        }
        _ => {
            print_omp_help();
            Ok(2)
        }
    }
}

fn print_omp_help() {
    eprintln!("usage: herdr omp guest-bridge <pane-id> <omp-session-id> <route-generation>");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_guest_bridge_generation_is_a_usage_error() {
        assert_eq!(
            run_omp_command(&[
                "guest-bridge".into(),
                "w1:p1".into(),
                "session".into(),
                "not-a-number".into(),
            ])
            .unwrap(),
            2
        );
    }
}
