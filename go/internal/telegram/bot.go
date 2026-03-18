package telegram

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"net/http"
	"strings"
	"time"
)

// Bot is the Telegram getUpdates long-poller.
type Bot struct {
	token       string
	client      *http.Client
	messenger   *Messenger
	offset      int64
	pollTimeout int
	botUsername string

	// Handler is called for each incoming message that isn't a slash command.
	OnMessage func(ctx context.Context, msg IncomingMessage)
	// OnCommand is called for slash commands (/help, /model, /reset, /status).
	OnCommand func(ctx context.Context, cmd Command)
	// OnCallback is called for inline keyboard button presses.
	OnCallback func(ctx context.Context, cb Callback)
}

type IncomingMessage struct {
	ChatID     int64
	MessageID  int64
	SenderID   int64
	SenderName string
	Text       string
	IsGroup    bool
	ChatTitle  string
	MediaType  string // "photo", "document", "video", "voice", "sticker", "" for text
	FileID     string
	FileName   string
}

type Command struct {
	ChatID     int64
	SenderName string
	Command    string // e.g. "help", "model", "reset"
	Args       string
	IsGroup    bool
}

type Callback struct {
	QueryID    string
	ChatID     int64
	MessageID  int64
	SenderID   int64
	SenderName string
	Data       string // e.g. "model:claude-opus-4-6"
}

func NewBot(token string, messenger *Messenger) *Bot {
	return &Bot{
		token:       token,
		client:      &http.Client{Timeout: 40 * time.Second},
		messenger:   messenger,
		pollTimeout: 30,
	}
}

// Run starts the long-polling loop. Blocks until ctx is cancelled.
func (b *Bot) Run(ctx context.Context) error {
	// Fetch bot username for mention detection
	if err := b.fetchBotInfo(ctx); err != nil {
		slog.Warn("failed to fetch bot info", "err", err)
	} else {
		slog.Info("telegram bot started", "username", b.botUsername)
	}

	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
		}

		updates, err := b.getUpdates(ctx)
		if err != nil {
			if ctx.Err() != nil {
				return ctx.Err()
			}
			slog.Error("getUpdates failed", "err", err)
			time.Sleep(3 * time.Second)
			continue
		}

		for _, u := range updates {
			b.offset = u.UpdateID + 1
			b.handleUpdate(ctx, u)
		}
	}
}

func (b *Bot) handleUpdate(ctx context.Context, u update) {
	if u.CallbackQuery != nil {
		cb := u.CallbackQuery
		senderName := ""
		if cb.From.FirstName != nil {
			senderName = *cb.From.FirstName
		}
		var chatID, messageID int64
		if cb.Message != nil {
			chatID = cb.Message.Chat.ID
			messageID = cb.Message.MessageID
		}
		if b.OnCallback != nil {
			b.OnCallback(ctx, Callback{
				QueryID:    cb.ID,
				ChatID:     chatID,
				MessageID:  messageID,
				SenderID:   cb.From.ID,
				SenderName: senderName,
				Data:       stringOrEmpty(cb.Data),
			})
		}
		return
	}

	msg := u.Message
	if msg == nil {
		return
	}

	text := stringOrEmpty(msg.Text)
	if text == "" {
		text = stringOrEmpty(msg.Caption)
	}

	senderName := ""
	var senderID int64
	if msg.From != nil {
		if msg.From.FirstName != nil {
			senderName = *msg.From.FirstName
		}
		senderID = msg.From.ID
	}

	isGroup := msg.Chat.ChatType == "group" || msg.Chat.ChatType == "supergroup"
	chatTitle := stringOrEmpty(msg.Chat.Title)

	// Check for slash commands
	if cmd, args, ok := b.parseCommand(text, msg.Entities); ok {
		if b.OnCommand != nil {
			b.OnCommand(ctx, Command{
				ChatID:     msg.Chat.ID,
				SenderName: senderName,
				Command:    cmd,
				Args:       args,
				IsGroup:    isGroup,
			})
		}
		return
	}

	// Detect media
	mediaType, fileID, fileName := detectMedia(msg)

	if b.OnMessage != nil {
		b.OnMessage(ctx, IncomingMessage{
			ChatID:     msg.Chat.ID,
			MessageID:  msg.MessageID,
			SenderID:   senderID,
			SenderName: senderName,
			Text:       text,
			IsGroup:    isGroup,
			ChatTitle:  chatTitle,
			MediaType:  mediaType,
			FileID:     fileID,
			FileName:   fileName,
		})
	}
}

func (b *Bot) parseCommand(text string, entities []messageEntity) (cmd, args string, ok bool) {
	if text == "" || !strings.HasPrefix(text, "/") {
		return "", "", false
	}
	// Check entities for bot_command type
	hasCommandEntity := false
	for _, e := range entities {
		if e.EntityType == "bot_command" && e.Offset == 0 {
			hasCommandEntity = true
			break
		}
	}
	if !hasCommandEntity && len(entities) > 0 {
		return "", "", false
	}

	parts := strings.SplitN(text, " ", 2)
	rawCmd := strings.TrimPrefix(parts[0], "/")
	// Strip @botname suffix
	if idx := strings.Index(rawCmd, "@"); idx >= 0 {
		rawCmd = rawCmd[:idx]
	}

	args = ""
	if len(parts) > 1 {
		args = strings.TrimSpace(parts[1])
	}
	return rawCmd, args, true
}

func detectMedia(msg *message) (mediaType, fileID, fileName string) {
	if msg.Photo != nil && len(msg.Photo) > 0 {
		// Use the largest photo (last in array)
		return "photo", msg.Photo[len(msg.Photo)-1].FileID, ""
	}
	if msg.Document != nil {
		name := ""
		if msg.Document.FileName != nil {
			name = *msg.Document.FileName
		}
		return "document", msg.Document.FileID, name
	}
	if msg.Video != nil {
		return "video", "", ""
	}
	if msg.Voice != nil {
		return "voice", "", ""
	}
	if msg.Sticker != nil {
		return "sticker", "", ""
	}
	return "", "", ""
}

// Telegram Bot API types (internal)

type update struct {
	UpdateID      int64          `json:"update_id"`
	Message       *message       `json:"message"`
	CallbackQuery *callbackQuery `json:"callback_query"`
}

type message struct {
	MessageID int64           `json:"message_id"`
	Date      int64           `json:"date"`
	Chat      chat            `json:"chat"`
	From      *user           `json:"from"`
	Text      *string         `json:"text"`
	Caption   *string         `json:"caption"`
	Entities  []messageEntity `json:"entities"`
	Photo     []photoSize     `json:"photo"`
	Document  *document       `json:"document"`
	Video     json.RawMessage `json:"video"`
	Voice     json.RawMessage `json:"voice"`
	Audio     json.RawMessage `json:"audio"`
	Sticker   *sticker        `json:"sticker"`
}

type chat struct {
	ID        int64   `json:"id"`
	ChatType  string  `json:"type"`
	Title     *string `json:"title"`
	FirstName *string `json:"first_name"`
}

type user struct {
	ID        int64   `json:"id"`
	FirstName *string `json:"first_name"`
	Username  *string `json:"username"`
}

type messageEntity struct {
	EntityType string `json:"type"`
	Offset     int    `json:"offset"`
	Length     int    `json:"length"`
}

type photoSize struct {
	FileID   string `json:"file_id"`
	Width    int    `json:"width"`
	Height   int    `json:"height"`
	FileSize *int64 `json:"file_size"`
}

type document struct {
	FileID   string  `json:"file_id"`
	FileName *string `json:"file_name"`
}

type sticker struct {
	Emoji  *string `json:"emoji"`
	FileID string  `json:"file_id"`
}

type callbackQuery struct {
	ID      string   `json:"id"`
	From    user     `json:"from"`
	Message *message `json:"message"`
	Data    *string  `json:"data"`
}

type apiResponse[T any] struct {
	OK          bool    `json:"ok"`
	Result      T       `json:"result"`
	Description *string `json:"description"`
}

func (b *Bot) getUpdates(ctx context.Context) ([]update, error) {
	url := fmt.Sprintf("%s/bot%s/getUpdates?offset=%d&timeout=%d",
		telegramAPIBase, b.token, b.offset, b.pollTimeout)

	req, err := http.NewRequestWithContext(ctx, "GET", url, nil)
	if err != nil {
		return nil, err
	}

	resp, err := b.client.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	var envelope apiResponse[[]update]
	if err := json.NewDecoder(resp.Body).Decode(&envelope); err != nil {
		return nil, fmt.Errorf("parse getUpdates: %w", err)
	}
	if !envelope.OK {
		desc := "unknown"
		if envelope.Description != nil {
			desc = *envelope.Description
		}
		return nil, fmt.Errorf("getUpdates: %s", desc)
	}
	return envelope.Result, nil
}

func (b *Bot) fetchBotInfo(ctx context.Context) error {
	url := fmt.Sprintf("%s/bot%s/getMe", telegramAPIBase, b.token)
	req, err := http.NewRequestWithContext(ctx, "GET", url, nil)
	if err != nil {
		return err
	}
	resp, err := b.client.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()

	var envelope apiResponse[struct {
		ID       int64   `json:"id"`
		Username *string `json:"username"`
	}]
	if err := json.NewDecoder(resp.Body).Decode(&envelope); err != nil {
		return err
	}
	if envelope.Result.Username != nil {
		b.botUsername = *envelope.Result.Username
	}
	return nil
}

func stringOrEmpty(s *string) string {
	if s == nil {
		return ""
	}
	return *s
}
