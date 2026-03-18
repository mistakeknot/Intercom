package main

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"net/http"
	"os"
	"os/signal"
	"sync"
	"syscall"
	"time"

	"github.com/mistakeknot/intercom/internal/config"
	"github.com/mistakeknot/intercom/internal/db"
	"github.com/mistakeknot/intercom/internal/mcp"
	"github.com/mistakeknot/intercom/internal/outbox"
	"github.com/mistakeknot/intercom/internal/queue"
	"github.com/mistakeknot/intercom/internal/routing"
	"github.com/mistakeknot/intercom/internal/scheduler"
	"github.com/mistakeknot/intercom/internal/subprocess"
	"github.com/mistakeknot/intercom/internal/telegram"
	"github.com/spf13/cobra"
)

var version = "dev"

func main() {
	var configPath string

	root := &cobra.Command{Use: "intercomd"}
	root.PersistentFlags().StringVar(&configPath, "config", "config/intercom.toml", "config file path")

	root.AddCommand(&cobra.Command{
		Use: "version", Short: "Print version",
		Run: func(cmd *cobra.Command, args []string) { fmt.Println(version) },
	})

	root.AddCommand(serveCmd(&configPath))
	root.AddCommand(mcpServerCmd(&configPath))

	if err := root.Execute(); err != nil {
		os.Exit(1)
	}
}

func serveCmd(configPath *string) *cobra.Command {
	return &cobra.Command{
		Use: "serve", Short: "Start the daemon",
		RunE: func(cmd *cobra.Command, args []string) error {
			cfg, err := config.Load(*configPath)
			if err != nil {
				return fmt.Errorf("load config: %w", err)
			}

			ctx, cancel := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
			defer cancel()

			// --- Postgres ---
			var pool *db.Pool
			if cfg.Storage.PostgresDSN != "" {
				pool = db.NewPool(cfg.Storage.PostgresDSN)
				if err := pool.Connect(ctx); err != nil {
					return fmt.Errorf("postgres: %w", err)
				}
				defer pool.Close(ctx)
			}

			// --- Telegram ---
			botToken := os.Getenv("TELEGRAM_BOT_TOKEN")
			messenger := telegram.NewMessenger(botToken)

			// --- Router ---
			router := routing.NewRouter(
				cfg.Runtimes.Profiles[cfg.Runtimes.DefaultRuntime].DefaultModel,
				cfg.Runtimes.DefaultRuntime,
			)

			// --- Subprocess manager ---
			mgr := subprocess.NewManager(cfg.Orchestrator.MaxConcurrentContainers)

			// --- Message queue ---
			msgQueue := queue.New(func(ctx context.Context, chatJID string) bool {
				return processGroupMessages(ctx, chatJID, cfg, pool, messenger, router, mgr)
			})

			// --- HTTP server ---
			mux := http.NewServeMux()
			mux.HandleFunc("/health", func(w http.ResponseWriter, r *http.Request) {
				w.Write([]byte("ok"))
			})
			mux.HandleFunc("/readyz", func(w http.ResponseWriter, r *http.Request) {
				status := map[string]any{
					"version":          version,
					"active_processes": mgr.ActiveCount(),
					"queue_pending":    msgQueue.PendingCount(),
					"queue_running":    msgQueue.RunningCount(),
				}
				if pool != nil {
					stats, err := pool.OutboxStats(ctx)
					if err == nil {
						status["outbox"] = stats
					}
				}
				w.Header().Set("Content-Type", "application/json")
				json.NewEncoder(w).Encode(status)
			})

			// --- Start background loops ---
			var wg sync.WaitGroup

			// HTTP server
			srv := &http.Server{Addr: cfg.Server.Bind, Handler: mux}
			wg.Add(1)
			go func() {
				defer wg.Done()
				slog.Info("http server starting", "bind", cfg.Server.Bind)
				if err := srv.ListenAndServe(); err != http.ErrServerClosed {
					slog.Error("http server", "err", err)
				}
			}()

			// Telegram poller
			if botToken != "" {
				bot := telegram.NewBot(botToken, messenger)
				bot.OnMessage = func(ctx context.Context, msg telegram.IncomingMessage) {
					handleTelegramMessage(ctx, msg, cfg, pool, msgQueue)
				}
				bot.OnCommand = func(ctx context.Context, cmd telegram.Command) {
					handleTelegramCommand(ctx, cmd, cfg, pool, messenger)
				}
				bot.OnCallback = func(ctx context.Context, cb telegram.Callback) {
					handleTelegramCallback(ctx, cb, cfg, pool, messenger)
				}
				wg.Add(1)
				go func() {
					defer wg.Done()
					if err := bot.Run(ctx); err != nil && ctx.Err() == nil {
						slog.Error("telegram poller", "err", err)
					}
				}()
			}

			// Message queue
			if cfg.Orchestrator.Enabled {
				wg.Add(1)
				go func() {
					defer wg.Done()
					msgQueue.Run(ctx)
				}()
			}

			// Scheduler
			if cfg.Scheduler.Enabled && pool != nil {
				sched := scheduler.New(pool, func(ctx context.Context, task db.ScheduledTask) (string, error) {
					return executeScheduledTask(ctx, task, cfg, pool, messenger, router, mgr)
				}, cfg.Scheduler.PollIntervalMs)
				wg.Add(1)
				go func() {
					defer wg.Done()
					sched.Run(ctx)
				}()
			}

			// Outbox drain (LISTEN/NOTIFY + fallback poll)
			if cfg.Orchestrator.UseOutbox && pool != nil && cfg.Storage.PostgresDSN != "" {
				drainSignal := make(chan struct{}, 1)
				drainer := outbox.NewDrainer(pool, msgQueue)

				// LISTEN loop — dedicated connection
				wg.Add(1)
				go func() {
					defer wg.Done()
					if err := db.ListenLoop(ctx, cfg.Storage.PostgresDSN, drainSignal); err != nil && ctx.Err() == nil {
						slog.Error("LISTEN loop", "err", err)
					}
				}()

				// Drain loop — processes claimed rows
				wg.Add(1)
				go func() {
					defer wg.Done()
					drainer.Run(ctx, drainSignal)
				}()

				// Cleanup loop — deletes old delivered rows
				wg.Add(1)
				go func() {
					defer wg.Done()
					outbox.RunCleanup(ctx, pool)
				}()
			}

			slog.Info("intercomd started", "version", version, "bind", cfg.Server.Bind,
				"telegram", botToken != "", "orchestrator", cfg.Orchestrator.Enabled,
				"scheduler", cfg.Scheduler.Enabled, "outbox", cfg.Orchestrator.UseOutbox)

			// Wait for shutdown
			<-ctx.Done()
			slog.Info("shutting down")

			// Graceful shutdown
			shutCtx, shutCancel := context.WithTimeout(context.Background(), 30*time.Second)
			defer shutCancel()
			srv.Shutdown(shutCtx)
			mgr.KillAll()
			mgr.WaitAll(10 * time.Second)
			wg.Wait()

			return nil
		},
	}
}

func mcpServerCmd(configPath *string) *cobra.Command {
	return &cobra.Command{
		Use: "mcp-server", Short: "Run as MCP server over stdio",
		RunE: func(cmd *cobra.Command, args []string) error {
			cfg, err := config.Load(*configPath)
			if err != nil {
				return fmt.Errorf("load config: %w", err)
			}

			ctx, cancel := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
			defer cancel()

			var pool *db.Pool
			if cfg.Storage.PostgresDSN != "" {
				pool = db.NewPool(cfg.Storage.PostgresDSN)
				if err := pool.Connect(ctx); err != nil {
					return fmt.Errorf("postgres: %w", err)
				}
				defer pool.Close(ctx)
			}

			botToken := os.Getenv("TELEGRAM_BOT_TOKEN")
			messenger := telegram.NewMessenger(botToken)

			srv := mcp.NewServer()
			if pool != nil {
				mcp.RegisterIntercomTools(srv, pool, messenger)
			}
			if cfg.Demarch.Enabled {
				mcp.RegisterDemarchTools(srv, cfg.Demarch.ReadAllowlist, cfg.Demarch.WriteAllowlist)
			}

			return srv.Run(ctx)
		},
	}
}

// --- Handler stubs wired in Task 10 ---
// These are minimal implementations. Full logic will be ported from Rust
// in follow-up tasks, but the wiring is complete.

func handleTelegramMessage(ctx context.Context, msg telegram.IncomingMessage, cfg *config.Config, pool *db.Pool, q *queue.Queue) {
	if pool == nil {
		return
	}
	// Store message
	chatJID := fmt.Sprintf("tg:%d", msg.ChatID)
	dbMsg := &db.Message{
		ID:         fmt.Sprintf("%d", msg.MessageID),
		ChatJID:    chatJID,
		Sender:     fmt.Sprintf("%d", msg.SenderID),
		SenderName: msg.SenderName,
		Content:    msg.Text,
		Timestamp:  time.Now().UTC().Format(time.RFC3339Nano),
	}
	if err := pool.StoreMessage(ctx, dbMsg); err != nil {
		slog.Error("store message", "err", err)
	}

	// Store chat metadata
	name := msg.ChatTitle
	if name == "" {
		name = msg.SenderName
	}
	isGroup := msg.IsGroup
	pool.StoreChatMetadata(ctx, chatJID, dbMsg.Timestamp, &name, nil, &isGroup)

	// Enqueue for processing
	q.Enqueue(chatJID)
}

func handleTelegramCommand(ctx context.Context, cmd telegram.Command, cfg *config.Config, pool *db.Pool, messenger *telegram.Messenger) {
	chatJID := fmt.Sprintf("tg:%d", cmd.ChatID)
	currentModel := cfg.Runtimes.Profiles[cfg.Runtimes.DefaultRuntime].DefaultModel

	switch cmd.Command {
	case "help":
		text := telegram.HandleHelp("Amtiskaw", currentModel)
		messenger.SendText(ctx, chatJID, text)
	case "model":
		if cmd.Args == "" {
			text, buttons := telegram.HandleModelList(currentModel)
			messenger.SendWithButtons(ctx, chatJID, text, buttons)
		} else {
			m := telegram.FindModel(cmd.Args)
			if m == nil {
				messenger.SendText(ctx, chatJID, fmt.Sprintf("Unknown model: %s", cmd.Args))
			} else {
				if pool != nil {
					pool.SetRouterState(ctx, "active_model", m.ID)
				}
				messenger.SendText(ctx, chatJID, fmt.Sprintf("Switched to %s", m.DisplayName))
			}
		}
	case "reset", "new":
		if pool != nil {
			// Find group folder for this chat
			groups, _ := pool.GetAllRegisteredGroups(ctx)
			for _, g := range groups {
				if g.JID == chatJID {
					pool.DeleteSession(ctx, g.Folder)
					break
				}
			}
		}
		messenger.SendText(ctx, chatJID, "Conversation reset.")
	case "status":
		messenger.SendText(ctx, chatJID, fmt.Sprintf("intercomd %s — running", version))
	}
}

func handleTelegramCallback(ctx context.Context, cb telegram.Callback, cfg *config.Config, pool *db.Pool, messenger *telegram.Messenger) {
	// Acknowledge the callback
	messenger.AnswerCallbackQuery(ctx, cb.QueryID, nil)

	// Parse callback data
	if len(cb.Data) < 2 {
		return
	}
	parts := splitFirst(cb.Data, ':')
	if len(parts) != 2 {
		return
	}

	switch parts[0] {
	case "model":
		modelID := parts[1]
		m := telegram.FindModel(modelID)
		if m == nil {
			return
		}
		if pool != nil {
			pool.SetRouterState(ctx, "active_model", m.ID)
		}
		chatJID := fmt.Sprintf("tg:%d", cb.ChatID)
		messenger.EditText(ctx, chatJID, fmt.Sprintf("%d", cb.MessageID),
			fmt.Sprintf("Switched to %s", m.DisplayName))
	case "approve", "reject":
		// Gate override handling — to be implemented
		slog.Info("callback", "action", parts[0], "id", parts[1])
	}
}

func processGroupMessages(ctx context.Context, chatJID string, cfg *config.Config, pool *db.Pool, messenger *telegram.Messenger, router routing.Router, mgr *subprocess.Manager) bool {
	if pool == nil {
		return false
	}

	// Look up registered group
	groups, err := pool.GetAllRegisteredGroups(ctx)
	if err != nil {
		slog.Error("get groups", "err", err)
		return false
	}
	var group *db.RegisteredGroup
	for _, g := range groups {
		if g.JID == chatJID {
			group = &g
			break
		}
	}
	if group == nil {
		slog.Debug("unregistered chat, ignoring", "chat_jid", chatJID)
		return false
	}

	// Get pending messages
	msgs, err := pool.CountPendingMessages(ctx, chatJID, "", "bot")
	if err != nil || msgs == 0 {
		return false
	}

	// Select model
	model, runtime, err := router.SelectModel(ctx, "default")
	if err != nil {
		slog.Error("select model", "err", err)
		return false
	}

	// Acquire subprocess slot
	if err := mgr.Acquire(group.Folder); err != nil {
		slog.Warn("subprocess slot", "err", err, "group", group.Folder)
		return false
	}
	defer mgr.Release(group.Folder)

	// Send typing indicator
	messenger.SendTyping(ctx, chatJID)

	// Get recent conversation for context
	conversation, _ := pool.GetRecentConversation(ctx, chatJID, 20)
	prompt := formatPrompt(conversation)

	// Start agent subprocess
	proc, err := subprocess.Start(ctx, subprocess.StartConfig{
		Runtime: runtime,
		Model:   model,
		WorkDir: fmt.Sprintf("%s/%s", cfg.Storage.GroupsDir, group.Folder),
		Prompt:  prompt,
	})
	if err != nil {
		slog.Error("start subprocess", "err", err, "group", group.Folder)
		return false
	}
	mgr.Register(group.Folder, proc)

	// Read output
	var result string
	proc.ReadFrames(ctx, func(frame subprocess.Frame) {
		if frame.Type == "result" || frame.Type == "text" {
			result += frame.Content
		}
	})
	proc.Wait()

	// Send result
	if result != "" {
		ids, err := messenger.SendText(ctx, chatJID, result)
		if err != nil {
			slog.Error("send result", "err", err)
			return false
		}
		// Store bot response
		for _, id := range ids {
			pool.StoreMessage(ctx, &db.Message{
				ID:           id,
				ChatJID:      chatJID,
				Sender:       "bot",
				SenderName:   "Amtiskaw",
				Content:      result,
				Timestamp:    time.Now().UTC().Format(time.RFC3339Nano),
				IsBotMessage: true,
			})
		}
	}

	return true
}

func executeScheduledTask(ctx context.Context, task db.ScheduledTask, cfg *config.Config, pool *db.Pool, messenger *telegram.Messenger, router routing.Router, mgr *subprocess.Manager) (string, error) {
	model, runtime, err := router.SelectModel(ctx, "scheduled")
	if err != nil {
		return "", err
	}

	proc, err := subprocess.Start(ctx, subprocess.StartConfig{
		Runtime: runtime,
		Model:   model,
		WorkDir: fmt.Sprintf("%s/%s", cfg.Storage.GroupsDir, task.GroupFolder),
		Prompt:  task.Prompt,
	})
	if err != nil {
		return "", err
	}

	var result string
	proc.ReadFrames(ctx, func(frame subprocess.Frame) {
		if frame.Type == "result" || frame.Type == "text" {
			result += frame.Content
		}
	})
	proc.Wait()

	// Send result to chat if configured
	if task.ChatJID != "" && result != "" {
		messenger.SendText(ctx, task.ChatJID, result)
	}

	return result, nil
}

func formatPrompt(msgs []db.ConversationMessage) string {
	if len(msgs) == 0 {
		return ""
	}
	var b []byte
	for _, m := range msgs {
		b = append(b, fmt.Sprintf("[%s] %s: %s\n", m.Timestamp, m.SenderName, m.Content)...)
	}
	return string(b)
}

func splitFirst(s string, sep byte) []string {
	idx := -1
	for i := 0; i < len(s); i++ {
		if s[i] == sep {
			idx = i
			break
		}
	}
	if idx < 0 {
		return []string{s}
	}
	return []string{s[:idx], s[idx+1:]}
}
