package db

import "encoding/json"

type Message struct {
	ID           string `json:"id"`
	ChatJID      string `json:"chat_jid"`
	Sender       string `json:"sender"`
	SenderName   string `json:"sender_name"`
	Content      string `json:"content"`
	Timestamp    string `json:"timestamp"`
	IsFromMe     bool   `json:"is_from_me"`
	IsBotMessage bool   `json:"is_bot_message"`
}

type ChatInfo struct {
	JID             string `json:"jid"`
	Name            string `json:"name"`
	LastMessageTime string `json:"last_message_time"`
	Channel         string `json:"channel,omitempty"`
	IsGroup         bool   `json:"is_group"`
}

type ConversationMessage struct {
	SenderName   string `json:"sender_name"`
	Content      string `json:"content"`
	Timestamp    string `json:"timestamp"`
	IsBotMessage bool   `json:"is_bot_message"`
}

type RegisteredGroup struct {
	JID             string          `json:"jid"`
	Name            string          `json:"name"`
	Folder          string          `json:"folder"`
	TriggerPattern  string          `json:"trigger"`
	AddedAt         string          `json:"added_at"`
	ContainerConfig json.RawMessage `json:"container_config,omitempty"`
	RequiresTrigger *bool           `json:"requires_trigger,omitempty"`
	Runtime         *string         `json:"runtime,omitempty"`
	Model           *string         `json:"model,omitempty"`
}

type ScheduledTask struct {
	ID            string  `json:"id"`
	GroupFolder   string  `json:"group_folder"`
	ChatJID       string  `json:"chat_jid"`
	Prompt        string  `json:"prompt"`
	ScheduleType  string  `json:"schedule_type"`
	ScheduleValue string  `json:"schedule_value"`
	ContextMode   string  `json:"context_mode"`
	NextRun       *string `json:"next_run,omitempty"`
	LastRun       *string `json:"last_run,omitempty"`
	LastResult    *string `json:"last_result,omitempty"`
	Status        string  `json:"status"`
	CreatedAt     string  `json:"created_at"`
}

type TaskUpdate struct {
	Prompt        *string `json:"prompt,omitempty"`
	ScheduleType  *string `json:"schedule_type,omitempty"`
	ScheduleValue *string `json:"schedule_value,omitempty"`
	NextRun       *string `json:"next_run,omitempty"`
	Status        *string `json:"status,omitempty"`
}

type TaskRunLog struct {
	TaskID     string  `json:"task_id"`
	RunAt      string  `json:"run_at"`
	DurationMs int64   `json:"duration_ms"`
	Status     string  `json:"status"`
	Result     *string `json:"result,omitempty"`
	Error      *string `json:"error,omitempty"`
}

type OutboxRow struct {
	ID          int64           `json:"id"`
	ChatJID     string          `json:"chat_jid"`
	PayloadType string          `json:"payload_type"`
	Payload     json.RawMessage `json:"payload"`
	Status      string          `json:"status"`
	CreatedAt   string          `json:"created_at"`
	Attempts    int32           `json:"attempts"`
}

type OutboxStats struct {
	Pending    int64 `json:"pending"`
	Processing int64 `json:"processing"`
	Failed     int64 `json:"failed"`
}
