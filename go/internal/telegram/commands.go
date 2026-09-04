package telegram

import (
	"fmt"
	"strings"
)

type ModelEntry struct {
	ID          string `json:"id"`
	Runtime     string `json:"runtime"`
	DisplayName string `json:"display_name"`
}

const DefaultModel = "claude-opus-4-6"
const DefaultRuntime = "claude"

func ModelCatalog() []ModelEntry {
	return []ModelEntry{
		{ID: "claude-opus-4-6", Runtime: "claude", DisplayName: "Claude Opus 4.6"},
		{ID: "claude-sonnet-4-6", Runtime: "claude", DisplayName: "Claude Sonnet 4.6"},
		{ID: "gemini-3.1-pro", Runtime: "gemini", DisplayName: "Gemini 3.1 Pro"},
		{ID: "gemini-2.5-flash", Runtime: "gemini", DisplayName: "Gemini 2.5 Flash"},
		{ID: "gpt-5.3-codex", Runtime: "codex", DisplayName: "GPT-5.3 Codex"},
		{ID: "gpt-6-astra", Runtime: "codex", DisplayName: "GPT-6 Astra (opt-in)"},
	}
}

func FindModel(id string) *ModelEntry {
	for _, m := range ModelCatalog() {
		if m.ID == id {
			return &m
		}
	}
	return nil
}

// RuntimeForModel infers the runtime backend from a model ID.
func RuntimeForModel(modelID string) string {
	if m := FindModel(modelID); m != nil {
		return m.Runtime
	}
	id := strings.ToLower(modelID)
	switch {
	case strings.HasPrefix(id, "claude-"):
		return "claude"
	case strings.HasPrefix(id, "gemini-"):
		return "gemini"
	case strings.HasPrefix(id, "gpt-"),
		strings.HasPrefix(id, "codex-"),
		strings.HasPrefix(id, "o1-"),
		strings.HasPrefix(id, "o3-"),
		strings.HasPrefix(id, "o4-"):
		return "codex"
	}
	return DefaultRuntime
}

// HandleHelp returns the /help response text.
func HandleHelp(assistantName, currentModel string) string {
	return fmt.Sprintf(`%s — AI assistant

Commands:
  /help — Show this message
  /model — View or change the AI model
  /model <name> — Switch to a specific model
  /reset — Start a new conversation
  /status — Show system status

Current model: %s`, assistantName, currentModel)
}

// HandleModelList returns the model selection text with inline keyboard buttons.
func HandleModelList(currentModel string) (string, [][]InlineButton) {
	catalog := ModelCatalog()
	text := fmt.Sprintf("Current model: %s\n\nAvailable models:", currentModel)
	var rows [][]InlineButton
	for _, m := range catalog {
		marker := ""
		if m.ID == currentModel {
			marker = " ✓"
		}
		rows = append(rows, []InlineButton{{
			Text:         m.DisplayName + marker,
			CallbackData: "model:" + m.ID,
		}})
	}
	return text, rows
}
