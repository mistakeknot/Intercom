package telegram

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"net/http"
	"strings"
)

const MaxTextChars = 4096
const telegramAPIBase = "https://api.telegram.org"

// Messenger handles Telegram message delivery: send, edit, chunking, buttons.
type Messenger struct {
	client   *http.Client
	botToken string
}

func NewMessenger(botToken string) *Messenger {
	return &Messenger{client: &http.Client{}, botToken: botToken}
}

func (m *Messenger) IsEnabled() bool { return m.botToken != "" }

// SendText sends a text message, chunking if > 4096 chars.
func (m *Messenger) SendText(ctx context.Context, chatID, text string) ([]string, error) {
	if strings.TrimSpace(text) == "" {
		return nil, fmt.Errorf("cannot send an empty Telegram message")
	}
	chunks := splitForTelegram(text, MaxTextChars)
	var messageIDs []string
	for _, chunk := range chunks {
		id, err := m.sendSingle(ctx, chatID, chunk, nil)
		if err != nil {
			return messageIDs, err
		}
		messageIDs = append(messageIDs, id)
	}
	return messageIDs, nil
}

// SendWithButtons sends a message with an inline keyboard.
func (m *Messenger) SendWithButtons(ctx context.Context, chatID, text string, keyboard [][]InlineButton) (string, error) {
	markup := map[string]any{"inline_keyboard": keyboard}
	return m.sendSingle(ctx, chatID, text, markup)
}

// EditText edits an existing message, truncating if > 4096 chars.
func (m *Messenger) EditText(ctx context.Context, chatID, messageID, text string) (bool, error) {
	truncated, wasTruncated := truncateForTelegram(text, MaxTextChars)
	body := map[string]any{
		"chat_id":    normalizeChatID(chatID),
		"message_id": messageID,
		"text":       truncated,
	}
	_, err := m.apiCall(ctx, "editMessageText", body)
	return wasTruncated, err
}

// SendTyping sends a "typing..." indicator. Fire-and-forget.
func (m *Messenger) SendTyping(ctx context.Context, chatID string) {
	body := map[string]any{
		"chat_id": normalizeChatID(chatID),
		"action":  "typing",
	}
	if _, err := m.apiCall(ctx, "sendChatAction", body); err != nil {
		slog.Debug("sendChatAction failed", "chat_id", chatID, "err", err)
	}
}

// AnswerCallbackQuery acknowledges a button press.
func (m *Messenger) AnswerCallbackQuery(ctx context.Context, queryID string, text *string) error {
	body := map[string]any{"callback_query_id": queryID}
	if text != nil {
		body["text"] = *text
	}
	_, err := m.apiCall(ctx, "answerCallbackQuery", body)
	return err
}

type InlineButton struct {
	Text         string `json:"text"`
	CallbackData string `json:"callback_data"`
}

func (m *Messenger) sendSingle(ctx context.Context, chatID, text string, replyMarkup any) (string, error) {
	body := map[string]any{
		"chat_id": normalizeChatID(chatID),
		"text":    text,
	}
	if replyMarkup != nil {
		body["reply_markup"] = replyMarkup
	}
	result, err := m.apiCall(ctx, "sendMessage", body)
	if err != nil {
		return "", err
	}
	// Extract message_id from result
	if msgID, ok := result["message_id"]; ok {
		return fmt.Sprintf("%v", msgID), nil
	}
	return "", nil
}

type apiEnvelope struct {
	OK          bool            `json:"ok"`
	Result      json.RawMessage `json:"result"`
	Description *string         `json:"description"`
}

func (m *Messenger) apiCall(ctx context.Context, method string, body map[string]any) (map[string]any, error) {
	url := fmt.Sprintf("%s/bot%s/%s", telegramAPIBase, m.botToken, method)
	payload, _ := json.Marshal(body)

	req, err := http.NewRequestWithContext(ctx, "POST", url, strings.NewReader(string(payload)))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := m.client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("telegram %s: %w", method, err)
	}
	defer resp.Body.Close()

	var envelope apiEnvelope
	if err := json.NewDecoder(resp.Body).Decode(&envelope); err != nil {
		return nil, fmt.Errorf("telegram %s: parse response: %w", method, err)
	}
	if !envelope.OK {
		desc := "unknown error"
		if envelope.Description != nil {
			desc = *envelope.Description
		}
		return nil, fmt.Errorf("telegram %s: %s", method, desc)
	}

	var result map[string]any
	if envelope.Result != nil {
		json.Unmarshal(envelope.Result, &result)
	}
	return result, nil
}

func normalizeChatID(jid string) string {
	return strings.TrimPrefix(jid, "tg:")
}

func splitForTelegram(text string, maxChars int) []string {
	runes := []rune(text)
	if len(runes) <= maxChars {
		return []string{text}
	}
	var chunks []string
	for len(runes) > 0 {
		end := maxChars
		if end > len(runes) {
			end = len(runes)
		}
		chunks = append(chunks, string(runes[:end]))
		runes = runes[end:]
	}
	return chunks
}

func truncateForTelegram(text string, maxChars int) (string, bool) {
	runes := []rune(text)
	if len(runes) <= maxChars {
		return text, false
	}
	return string(runes[:maxChars]), true
}
