package routing

import (
	"context"
	"encoding/json"
	"fmt"
	"os/exec"
	"strings"
	"time"
)

// Router selects which model/runtime to use for a given phase.
type Router interface {
	SelectModel(ctx context.Context, phase string) (model, runtime string, err error)
}

// ConfigRouter always returns the model from config. NoOp fallback.
type ConfigRouter struct {
	Model   string
	Runtime string
}

func (r *ConfigRouter) SelectModel(_ context.Context, _ string) (string, string, error) {
	return r.Model, r.Runtime, nil
}

// IntercoreRouter shells out to `ic route model` — same pattern Skaffen uses.
type IntercoreRouter struct {
	Timeout time.Duration
}

func NewIntercoreRouter() *IntercoreRouter {
	return &IntercoreRouter{Timeout: 5 * time.Second}
}

// icRouteResult mirrors `ic route model --json`, which emits
// {model, phase, category, agent}. ic does not report a runtime, so the
// runtime is inferred from the model ID.
type icRouteResult struct {
	Model    string `json:"model"`
	Phase    string `json:"phase"`
	Category string `json:"category"`
	Agent    string `json:"agent"`
}

func (r *IntercoreRouter) SelectModel(ctx context.Context, phase string) (string, string, error) {
	ctx, cancel := context.WithTimeout(ctx, r.Timeout)
	defer cancel()

	// ic parses only the --flag=value form; "--phase X" leaves phase unset.
	cmd := exec.CommandContext(ctx, "ic", "route", "model", "--phase="+phase, "--json")
	out, err := cmd.Output()
	if err != nil {
		return "", "", fmt.Errorf("ic route model: %w", err)
	}

	var result icRouteResult
	if err := json.Unmarshal(out, &result); err != nil {
		return "", "", fmt.Errorf("ic route model: parse: %w", err)
	}
	if result.Model == "" {
		return "", "", fmt.Errorf("ic route model: empty model")
	}
	return result.Model, RuntimeForModel(result.Model), nil
}

// FallbackRouter tries IntercoreRouter, falls back to ConfigRouter on error.
type FallbackRouter struct {
	Primary  Router
	Fallback Router
}

func (r *FallbackRouter) SelectModel(ctx context.Context, phase string) (string, string, error) {
	model, runtime, err := r.Primary.SelectModel(ctx, phase)
	if err == nil {
		return model, runtime, nil
	}
	return r.Fallback.SelectModel(ctx, phase)
}

// NewRouter creates the appropriate router based on whether `ic` is available.
func NewRouter(defaultModel, defaultRuntime string) Router {
	config := &ConfigRouter{Model: defaultModel, Runtime: defaultRuntime}

	// Check if ic binary is available
	if _, err := exec.LookPath("ic"); err != nil {
		return config
	}

	return &FallbackRouter{
		Primary:  NewIntercoreRouter(),
		Fallback: config,
	}
}

// RuntimeForModel infers runtime from model ID prefix.
func RuntimeForModel(modelID string) string {
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
	return "claude"
}
