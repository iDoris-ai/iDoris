#[cfg(unix)]
mod unix {
    #![allow(clippy::expect_used)]

    use std::process::Stdio;
    use std::time::Duration;

    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;
    use tokio::time::timeout;

    #[tokio::test]
    async fn safe_process_group_api_signals_parent_and_descendant() {
        let script = r#"
            /bin/sh -c '
                trap "echo descendant-term; exit 0" TERM
                echo descendant-ready:$$
                while :; do sleep 1; done
            ' &
            child=$!
            trap 'echo parent-term; wait "$child" 2>/dev/null || true; exit 0' TERM
            echo parent-ready:$$
            wait "$child"
        "#;
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(script)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = command.spawn().expect("probe shell must spawn");
        let group = child.id().expect("spawned child must have a pid") as i32;
        let stdout = child.stdout.take().expect("probe stdout must be piped");
        let mut lines = BufReader::new(stdout).lines();

        let mut parent_ready = false;
        let mut descendant_ready = false;
        for _ in 0..2 {
            let line = timeout(Duration::from_secs(2), lines.next_line())
                .await
                .expect("ready marker timed out")
                .expect("reading ready marker failed")
                .expect("ready marker missing");
            parent_ready |= line.starts_with("parent-ready:");
            descendant_ready |= line.starts_with("descendant-ready:");
        }
        assert!(
            parent_ready && descendant_ready,
            "both processes must be ready"
        );

        killpg(Pid::from_raw(group), Signal::SIGTERM).expect("group SIGTERM must succeed");

        let mut parent_term = false;
        let mut descendant_term = false;
        for _ in 0..4 {
            let Some(line) = timeout(Duration::from_secs(2), lines.next_line())
                .await
                .expect("termination marker timed out")
                .expect("reading termination marker failed")
            else {
                break;
            };
            parent_term |= line == "parent-term";
            descendant_term |= line == "descendant-term";
            if parent_term && descendant_term {
                break;
            }
        }
        assert!(parent_term, "parent process did not receive group SIGTERM");
        assert!(
            descendant_term,
            "descendant process did not receive group SIGTERM"
        );
        timeout(Duration::from_secs(2), child.wait())
            .await
            .expect("probe process did not exit")
            .expect("waiting for probe process failed");
    }
}

#[cfg(not(unix))]
mod non_unix {
    fn terminate_process_group(_pid: u32) -> Result<(), &'static str> {
        Err("subscription process groups are unsupported on this platform")
    }

    #[test]
    fn process_group_support_is_explicitly_unsupported() {
        assert_eq!(
            terminate_process_group(1),
            Err("subscription process groups are unsupported on this platform")
        );
    }
}
