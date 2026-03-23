from telegram_acp.config import Settings


def test_defaults():
    s = Settings(telegram_token="test-token")
    assert s.telegram_token == "test-token"
    assert s.acp_server_cmd == "kiro cli acp"
    assert s.acp_server_url == ""
    assert s.acp_session_mode == ""
    assert s.allowed_chats == ""
    assert s.debug_acp is False


def test_allowed_chat_ids_empty():
    s = Settings(telegram_token="t")
    assert s.allowed_chat_ids == set()


def test_allowed_chat_ids_csv():
    s = Settings(telegram_token="t", allowed_chats="123, -456, 789")
    assert s.allowed_chat_ids == {123, -456, 789}


def test_acp_server_argv():
    s = Settings(telegram_token="t", acp_server_cmd="kiro cli acp")
    assert s.acp_server_argv == ["kiro", "cli", "acp"]


def test_acp_server_base_url_empty():
    s = Settings(telegram_token="t")
    assert s.acp_server_base_url is None


def test_acp_server_base_url_strips_whitespace():
    s = Settings(telegram_token="t", acp_server_url="  https://example.test/acp  ")
    assert s.acp_server_base_url == "https://example.test/acp"
