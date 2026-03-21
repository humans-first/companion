// Allow all users in private chats.
permit(principal, action == Action::"PromptRequest", resource)
when { context.chat_type == "private" };

// Example: allow a specific user everywhere.
// permit(principal == User::"telegram|12345", action == Action::"PromptRequest", resource);

// Example: block a specific user.
// forbid(principal == User::"telegram|99999", action == Action::"PromptRequest", resource);
