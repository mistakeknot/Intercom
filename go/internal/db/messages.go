package db

import (
	"context"
	"fmt"
	"strings"
	"time"

	"github.com/jackc/pgx/v5"
)

func (p *Pool) StoreMessage(ctx context.Context, msg *Message) error {
	_, err := p.Exec(ctx, `
		INSERT INTO messages (id, chat_jid, sender, sender_name, content, timestamp, is_from_me, is_bot_message)
		VALUES ($1, $2, $3, $4, $5, $6::text::timestamptz, $7, $8)
		ON CONFLICT (id, chat_jid) DO UPDATE SET
		  content = EXCLUDED.content,
		  is_bot_message = EXCLUDED.is_bot_message`,
		msg.ID, msg.ChatJID, msg.Sender, msg.SenderName, msg.Content,
		msg.Timestamp, msg.IsFromMe, msg.IsBotMessage,
	)
	return err
}

func (p *Pool) StoreChatMetadata(ctx context.Context, jid, timestamp string, name, channel *string, isGroup *bool) error {
	displayName := jid
	if name != nil {
		displayName = *name
	}
	_, err := p.Exec(ctx, `
		INSERT INTO chats (jid, name, last_message_time, channel, is_group)
		VALUES ($1, $2, $3::text::timestamptz, $4, $5)
		ON CONFLICT (jid) DO UPDATE SET
		  name = COALESCE(NULLIF(EXCLUDED.name, EXCLUDED.jid), chats.name),
		  last_message_time = GREATEST(chats.last_message_time, EXCLUDED.last_message_time),
		  channel = COALESCE(EXCLUDED.channel, chats.channel),
		  is_group = COALESCE(EXCLUDED.is_group, chats.is_group)`,
		jid, displayName, timestamp, channel, isGroup,
	)
	return err
}

func (p *Pool) GetAllChats(ctx context.Context) ([]ChatInfo, error) {
	rows, err := p.Query(ctx,
		`SELECT jid, name, last_message_time, channel, is_group
		 FROM chats ORDER BY last_message_time DESC`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	return collectChats(rows)
}

func (p *Pool) GetRecentConversation(ctx context.Context, chatJID string, limit int) ([]ConversationMessage, error) {
	rows, err := p.Query(ctx, `
		SELECT sender_name, content, timestamp, is_bot_message
		FROM messages
		WHERE chat_jid = $1 AND content != '' AND content IS NOT NULL
		ORDER BY timestamp DESC
		LIMIT $2`, chatJID, limit)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var msgs []ConversationMessage
	for rows.Next() {
		var m ConversationMessage
		var ts time.Time
		if err := rows.Scan(&m.SenderName, &m.Content, &ts, &m.IsBotMessage); err != nil {
			return nil, err
		}
		m.Timestamp = ts.Format(time.RFC3339Nano)
		msgs = append(msgs, m)
	}
	// Reverse to chronological order
	for i, j := 0, len(msgs)-1; i < j; i, j = i+1, j-1 {
		msgs[i], msgs[j] = msgs[j], msgs[i]
	}
	return msgs, rows.Err()
}

func (p *Pool) GetNewMessages(ctx context.Context, jids []string, lastTimestamp, botPrefix string) ([]Message, string, error) {
	if len(jids) == 0 {
		return nil, lastTimestamp, nil
	}
	if lastTimestamp == "" {
		lastTimestamp = "1970-01-01T00:00:00Z"
	}
	botPattern := botPrefix + ":%"

	// Build dynamic IN clause
	args := []any{lastTimestamp}
	placeholders := make([]string, len(jids))
	for i, jid := range jids {
		args = append(args, jid)
		placeholders[i] = fmt.Sprintf("$%d", i+2)
	}
	botIdx := len(jids) + 2
	args = append(args, botPattern)

	sql := fmt.Sprintf(`
		SELECT id, chat_jid, sender, sender_name, content, timestamp
		FROM messages
		WHERE timestamp > $1::text::timestamptz AND chat_jid IN (%s)
		  AND is_bot_message = FALSE AND content NOT LIKE $%d
		  AND content != '' AND content IS NOT NULL
		ORDER BY timestamp`,
		strings.Join(placeholders, ", "), botIdx)

	rows, err := p.Query(ctx, sql, args...)
	if err != nil {
		return nil, lastTimestamp, err
	}
	defer rows.Close()

	newTS := lastTimestamp
	var msgs []Message
	for rows.Next() {
		var m Message
		var ts time.Time
		if err := rows.Scan(&m.ID, &m.ChatJID, &m.Sender, &m.SenderName, &m.Content, &ts); err != nil {
			return nil, lastTimestamp, err
		}
		m.Timestamp = ts.Format(time.RFC3339Nano)
		if m.Timestamp > newTS {
			newTS = m.Timestamp
		}
		msgs = append(msgs, m)
	}
	return msgs, newTS, rows.Err()
}

func (p *Pool) CountPendingMessages(ctx context.Context, chatJID, sinceTimestamp, botPrefix string) (int64, error) {
	if sinceTimestamp == "" {
		sinceTimestamp = "1970-01-01T00:00:00Z"
	}
	botPattern := botPrefix + ":%"
	var count int64
	err := p.QueryRow(ctx, `
		SELECT COUNT(*) FROM messages
		WHERE chat_jid = $1 AND timestamp > $2::text::timestamptz
		  AND is_bot_message = FALSE AND content NOT LIKE $3
		  AND content != '' AND content IS NOT NULL`,
		chatJID, sinceTimestamp, botPattern).Scan(&count)
	return count, err
}

func collectChats(rows pgx.Rows) ([]ChatInfo, error) {
	var chats []ChatInfo
	for rows.Next() {
		var c ChatInfo
		var ts time.Time
		var name, channel *string
		var isGroup *bool
		if err := rows.Scan(&c.JID, &name, &ts, &channel, &isGroup); err != nil {
			return nil, err
		}
		if name != nil {
			c.Name = *name
		}
		if channel != nil {
			c.Channel = *channel
		}
		if isGroup != nil {
			c.IsGroup = *isGroup
		}
		c.LastMessageTime = ts.Format(time.RFC3339Nano)
		chats = append(chats, c)
	}
	return chats, rows.Err()
}
