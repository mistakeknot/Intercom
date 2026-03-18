package mcp

import (
	"context"
	"encoding/json"
	"fmt"

	"github.com/mistakeknot/intercom/internal/db"
	"github.com/mistakeknot/intercom/internal/telegram"
)

// RegisterIntercomTools adds intercom-specific tools to the MCP server.
func RegisterIntercomTools(s *Server, pool *db.Pool, messenger *telegram.Messenger) {
	s.RegisterTool(Tool{
		Name:        "send_message",
		Description: "Send a text message to a Telegram chat",
		InputSchema: json.RawMessage(`{
			"type": "object",
			"properties": {
				"chat_id": {"type": "string", "description": "Telegram chat ID"},
				"text": {"type": "string", "description": "Message text to send"}
			},
			"required": ["chat_id", "text"]
		}`),
		Handler: func(ctx context.Context, input json.RawMessage) (any, error) {
			var args struct {
				ChatID string `json:"chat_id"`
				Text   string `json:"text"`
			}
			if err := json.Unmarshal(input, &args); err != nil {
				return nil, err
			}
			ids, err := messenger.SendText(ctx, args.ChatID, args.Text)
			if err != nil {
				return nil, err
			}
			return map[string]any{"message_ids": ids}, nil
		},
	})

	s.RegisterTool(Tool{
		Name:        "edit_message",
		Description: "Edit an existing Telegram message",
		InputSchema: json.RawMessage(`{
			"type": "object",
			"properties": {
				"chat_id": {"type": "string", "description": "Telegram chat ID"},
				"message_id": {"type": "string", "description": "Message ID to edit"},
				"text": {"type": "string", "description": "New message text"}
			},
			"required": ["chat_id", "message_id", "text"]
		}`),
		Handler: func(ctx context.Context, input json.RawMessage) (any, error) {
			var args struct {
				ChatID    string `json:"chat_id"`
				MessageID string `json:"message_id"`
				Text      string `json:"text"`
			}
			if err := json.Unmarshal(input, &args); err != nil {
				return nil, err
			}
			truncated, err := messenger.EditText(ctx, args.ChatID, args.MessageID, args.Text)
			if err != nil {
				return nil, err
			}
			return map[string]any{"truncated": truncated}, nil
		},
	})

	s.RegisterTool(Tool{
		Name:        "get_conversation",
		Description: "Get recent conversation messages from a chat",
		InputSchema: json.RawMessage(`{
			"type": "object",
			"properties": {
				"chat_id": {"type": "string", "description": "Telegram chat JID"},
				"limit": {"type": "integer", "description": "Max messages to return", "default": 50}
			},
			"required": ["chat_id"]
		}`),
		Handler: func(ctx context.Context, input json.RawMessage) (any, error) {
			var args struct {
				ChatID string `json:"chat_id"`
				Limit  int    `json:"limit"`
			}
			if err := json.Unmarshal(input, &args); err != nil {
				return nil, err
			}
			if args.Limit <= 0 {
				args.Limit = 50
			}
			msgs, err := pool.GetRecentConversation(ctx, args.ChatID, args.Limit)
			if err != nil {
				return nil, err
			}
			return msgs, nil
		},
	})

	s.RegisterTool(Tool{
		Name:        "list_chats",
		Description: "List all known chats",
		InputSchema: json.RawMessage(`{"type": "object", "properties": {}}`),
		Handler: func(ctx context.Context, _ json.RawMessage) (any, error) {
			return pool.GetAllChats(ctx)
		},
	})

	s.RegisterTool(Tool{
		Name:        "send_typing",
		Description: "Show typing indicator in a chat",
		InputSchema: json.RawMessage(`{
			"type": "object",
			"properties": {
				"chat_id": {"type": "string", "description": "Telegram chat ID"}
			},
			"required": ["chat_id"]
		}`),
		Handler: func(ctx context.Context, input json.RawMessage) (any, error) {
			var args struct {
				ChatID string `json:"chat_id"`
			}
			if err := json.Unmarshal(input, &args); err != nil {
				return nil, err
			}
			messenger.SendTyping(ctx, args.ChatID)
			return map[string]any{"ok": true}, nil
		},
	})

	s.RegisterTool(Tool{
		Name:        "send_with_buttons",
		Description: "Send a message with inline keyboard buttons",
		InputSchema: json.RawMessage(`{
			"type": "object",
			"properties": {
				"chat_id": {"type": "string", "description": "Telegram chat ID"},
				"text": {"type": "string", "description": "Message text"},
				"buttons": {
					"type": "array",
					"description": "Array of button rows",
					"items": {
						"type": "array",
						"items": {
							"type": "object",
							"properties": {
								"text": {"type": "string"},
								"callback_data": {"type": "string"}
							},
							"required": ["text", "callback_data"]
						}
					}
				}
			},
			"required": ["chat_id", "text", "buttons"]
		}`),
		Handler: func(ctx context.Context, input json.RawMessage) (any, error) {
			var args struct {
				ChatID  string                    `json:"chat_id"`
				Text    string                    `json:"text"`
				Buttons [][]telegram.InlineButton `json:"buttons"`
			}
			if err := json.Unmarshal(input, &args); err != nil {
				return nil, err
			}
			id, err := messenger.SendWithButtons(ctx, args.ChatID, args.Text, args.Buttons)
			if err != nil {
				return nil, fmt.Errorf("send_with_buttons: %w", err)
			}
			return map[string]any{"message_id": id}, nil
		},
	})
}
