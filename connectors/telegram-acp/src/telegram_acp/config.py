from pydantic_settings import BaseSettings, SettingsConfigDict


class Settings(BaseSettings):
    model_config = SettingsConfigDict(env_file=".env", env_file_encoding="utf-8")

    telegram_token: str

    # ACP server command (space-separated, will be split into argv)
    acp_server_cmd: str = "kiro cli acp"

    # ACP HTTP transport base URL. When set, telegram-acp connects over HTTP(S)
    # instead of spawning an ACP subprocess locally.
    acp_server_url: str = ""

    # ACP session mode to set after session creation (e.g. "code", "chat").
    # Leave empty to use the agent's default mode.
    acp_session_mode: str = ""

    # Allowlist: comma-separated chat IDs the bot will respond in.
    # Use /chatid in any chat to get its ID (groups are negative numbers).
    # Leave empty to respond in all chats (dev only).
    allowed_chats: str = ""

    # Log raw ACP ndjson messages
    debug_acp: bool = False

    @property
    def allowed_chat_ids(self) -> set[int]:
        if not self.allowed_chats.strip():
            return set()
        return {int(cid.strip()) for cid in self.allowed_chats.split(",") if cid.strip()}

    @property
    def acp_server_argv(self) -> list[str]:
        return self.acp_server_cmd.split()

    @property
    def acp_server_base_url(self) -> str | None:
        url = self.acp_server_url.strip()
        return url or None
