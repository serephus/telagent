use anyhow::Result;
use teloxide::{
    Bot,
    payloads::{SetChatMenuButtonSetters, SetMyDescriptionSetters, SetMyShortDescriptionSetters},
    prelude::Requester,
    types::{BotCommand, MenuButton},
};

pub struct CommandSpec {
    pub name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
}

pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "start",
        usage: "",
        description: "Show help",
    },
    CommandSpec {
        name: "bind",
        usage: "CODE",
        description: "Bind this chat with a pairing code",
    },
    CommandSpec {
        name: "unbind",
        usage: "",
        description: "Unbind this chat",
    },
    CommandSpec {
        name: "new",
        usage: "[DIRECTORY] [PROMPT]",
        description: "Start a new session with an optional prompt",
    },
    CommandSpec {
        name: "compact",
        usage: "[ID]",
        description: "Compact the active session context",
    },
    CommandSpec {
        name: "sessions",
        usage: "",
        description: "List recent sessions",
    },
    CommandSpec {
        name: "use",
        usage: "ID",
        description: "Select the active session",
    },
    CommandSpec {
        name: "cancel",
        usage: "",
        description: "Clear the active session",
    },
    CommandSpec {
        name: "status",
        usage: "",
        description: "Show bound sessions and state",
    },
    CommandSpec {
        name: "mute",
        usage: "[ID]",
        description: "Mute completion notifications",
    },
    CommandSpec {
        name: "unmute",
        usage: "[ID]",
        description: "Unmute completion notifications",
    },
    CommandSpec {
        name: "abort",
        usage: "[ID]",
        description: "Abort a running session",
    },
    CommandSpec {
        name: "help",
        usage: "",
        description: "Show available commands",
    },
];

pub fn help_text() -> String {
    let mut text = String::from("telagent\n\nCommands:\n");
    for command in COMMANDS {
        text.push('/');
        text.push_str(command.name);
        if !command.usage.is_empty() {
            text.push(' ');
            text.push_str(command.usage);
        }
        text.push('\n');
    }
    text.push_str("\nReply to a notification to send feedback.");
    text
}

pub fn bot_commands() -> Vec<BotCommand> {
    COMMANDS
        .iter()
        .map(|command| BotCommand::new(command.name, command.description))
        .collect()
}

pub async fn register(bot: &Bot) -> Result<()> {
    bot.set_my_commands(bot_commands()).await?;
    bot.set_my_description()
        .description("Telegram bridge for a local opencode server")
        .await?;
    bot.set_my_short_description()
        .short_description("Telegram bridge for opencode")
        .await?;
    bot.set_chat_menu_button()
        .menu_button(MenuButton::Commands)
        .await?;
    Ok(())
}
