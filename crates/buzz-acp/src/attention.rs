//! Optional managed-attention inventory at the actual ACP prompt boundary.
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;

pub(crate) async fn prompt(context: &Value) -> Option<String> {
    let Ok(command) = std::env::var("BUZZ_ATTENTION_COMMAND") else {
        return None;
    };
    let content = match inventory(&command, context).await {
        Ok(inventory) => format!("Owned commitment inventory: {inventory}"),
        Err(error) => format!("Owned commitment inventory unavailable: {error}. This is not an empty set; additive authoring remains blocked until it can be reviewed."),
    };
    let text = format!(
        "Origin supplied by Buzz: {context}\n{content}\n\
         Use edamame-watch for all future attention. Pass the originating link with --context when it is not already in your environment. \
         Create, update, renew, rearm, snooze and completion return the owned set for explicit review before writing. \
         A triggering reminder needs a disposition: complete/cancel, or reschedule to an explicit future boundary. \
         Decide separately whether a parent monitor still deserves its remaining lifetime."
    );
    Some(crate::prompt_framing::semantic_section(
        "attention-context",
        &text,
    ))
}

async fn inventory(command: &str, context: &Value) -> anyhow::Result<Value> {
    let link = context["link"].as_str();
    let policy = context["policy"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("scope policy is missing"))?;
    let mut process = tokio::process::Command::new(command);
    process.arg("list").env_remove("BUZZ_ATTENTION_LINK");
    if let Some(link) = link {
        process
            .args(["--context", link])
            .env("BUZZ_ATTENTION_LINK", link);
    }
    let mut child = process
        .env("BUZZ_ACP_SESSION_POLICY", policy)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("inventory stdout unavailable"))?;
    let mut reader = stdout.take(65537);
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(150), async {
        reader.read_to_end(&mut bytes).await?;
        anyhow::ensure!(
            bytes.len() <= 65536,
            "inventory exceeds 64 KiB; inspect it with edamame-watch"
        );
        let status = child.wait().await?;
        let value: Value = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            status.success() && value["complete"] == true,
            "inventory sources are incomplete"
        );
        Ok(value)
    })
    .await?
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn inventory_uses_real_command_with_origin_and_rejects_incomplete_sources() {
        let directory = tempfile::tempdir().unwrap();
        let command = directory.path().join("inventory");
        std::fs::write(
            &command,
            r#"#!/bin/sh
if [ "$1" != list ] || [ "$2" != --context ] || [ "$3" != "$BUZZ_ATTENTION_LINK" ]; then exit 8; fi
printf '{"complete":true,"watches":[{"id":"time:existing"}]}'
"#,
        )
        .unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
        let context =
            serde_json::json!({"link":"buzz://message?channel=origin&id=root", "policy":"thread"});
        let result = inventory(command.to_str().unwrap(), &context)
            .await
            .unwrap();
        assert_eq!(result["watches"][0]["id"], "time:existing");
        std::fs::write(
            &command,
            "#!/bin/sh\nprintf '{\"complete\":false,\"watches\":[]}'\n",
        )
        .unwrap();
        assert!(inventory(command.to_str().unwrap(), &context)
            .await
            .is_err());
        std::fs::write(&command, "#!/bin/sh\nhead -c 65537 /dev/zero\n").unwrap();
        assert!(inventory(command.to_str().unwrap(), &context)
            .await
            .unwrap_err()
            .to_string()
            .contains("64 KiB"));
    }

    #[tokio::test]
    async fn legacy_originless_reminder_still_reads_own_inventory() {
        let directory = tempfile::tempdir().unwrap();
        let command = directory.path().join("inventory");
        std::fs::write(&command, "#!/bin/sh\n[ \"$#\" = 1 ] || exit 8\nprintf '{\"complete\":true,\"watches\":[{\"origin\":\"unassigned\"}]}'\n").unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
        let result = inventory(
            command.to_str().unwrap(),
            &serde_json::json!({"policy":"thread"}),
        )
        .await
        .unwrap();
        assert_eq!(result["watches"][0]["origin"], "unassigned");
    }
}
