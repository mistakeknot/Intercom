// Package mcp implements a JSON-RPC 2.0 MCP server over stdio.
//
// This server is launched per-agent as a subprocess. The agent connects via
// --mcp-config pointing to a temporary config that invokes `intercomd mcp-server`.
// The server exposes intercom and demarch tools to the agent.
package mcp

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"os"
)

// Server is a JSON-RPC 2.0 MCP server that reads from stdin and writes to stdout.
type Server struct {
	tools   map[string]Tool
	scanner *bufio.Scanner
	writer  io.Writer
}

// Tool is a registered MCP tool.
type Tool struct {
	Name        string          `json:"name"`
	Description string          `json:"description"`
	InputSchema json.RawMessage `json:"inputSchema"`
	Handler     ToolHandler     `json:"-"`
}

// ToolHandler processes a tool call and returns the result.
type ToolHandler func(ctx context.Context, input json.RawMessage) (any, error)

// Request is a JSON-RPC 2.0 request.
type Request struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      any             `json:"id"`
	Method  string          `json:"method"`
	Params  json.RawMessage `json:"params,omitempty"`
}

// Response is a JSON-RPC 2.0 response.
type Response struct {
	JSONRPC string `json:"jsonrpc"`
	ID      any    `json:"id"`
	Result  any    `json:"result,omitempty"`
	Error   *Error `json:"error,omitempty"`
}

type Error struct {
	Code    int    `json:"code"`
	Message string `json:"message"`
}

func NewServer() *Server {
	return &Server{
		tools:   make(map[string]Tool),
		scanner: bufio.NewScanner(os.Stdin),
		writer:  os.Stdout,
	}
}

func (s *Server) RegisterTool(tool Tool) {
	s.tools[tool.Name] = tool
}

// Run reads JSON-RPC messages from stdin until EOF or ctx cancellation.
func (s *Server) Run(ctx context.Context) error {
	s.scanner.Buffer(make([]byte, 0, 1024*1024), 10*1024*1024)

	for s.scanner.Scan() {
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
		}

		line := s.scanner.Bytes()
		if len(line) == 0 {
			continue
		}

		var req Request
		if err := json.Unmarshal(line, &req); err != nil {
			s.writeResponse(Response{JSONRPC: "2.0", Error: &Error{Code: -32700, Message: "parse error"}})
			continue
		}

		resp := s.handleRequest(ctx, req)
		s.writeResponse(resp)
	}
	return s.scanner.Err()
}

func (s *Server) handleRequest(ctx context.Context, req Request) Response {
	switch req.Method {
	case "initialize":
		return Response{
			JSONRPC: "2.0",
			ID:      req.ID,
			Result: map[string]any{
				"protocolVersion": "2024-11-05",
				"capabilities": map[string]any{
					"tools": map[string]any{},
				},
				"serverInfo": map[string]any{
					"name":    "intercomd",
					"version": "1.0.0",
				},
			},
		}

	case "notifications/initialized":
		// No response needed for notifications
		return Response{JSONRPC: "2.0", ID: req.ID, Result: map[string]any{}}

	case "tools/list":
		tools := make([]map[string]any, 0, len(s.tools))
		for _, t := range s.tools {
			tools = append(tools, map[string]any{
				"name":        t.Name,
				"description": t.Description,
				"inputSchema": t.InputSchema,
			})
		}
		return Response{
			JSONRPC: "2.0",
			ID:      req.ID,
			Result:  map[string]any{"tools": tools},
		}

	case "tools/call":
		return s.handleToolCall(ctx, req)

	default:
		return Response{
			JSONRPC: "2.0",
			ID:      req.ID,
			Error:   &Error{Code: -32601, Message: fmt.Sprintf("method not found: %s", req.Method)},
		}
	}
}

func (s *Server) handleToolCall(ctx context.Context, req Request) Response {
	var params struct {
		Name      string          `json:"name"`
		Arguments json.RawMessage `json:"arguments"`
	}
	if err := json.Unmarshal(req.Params, &params); err != nil {
		return Response{
			JSONRPC: "2.0", ID: req.ID,
			Error: &Error{Code: -32602, Message: "invalid params"},
		}
	}

	tool, ok := s.tools[params.Name]
	if !ok {
		return Response{
			JSONRPC: "2.0", ID: req.ID,
			Error: &Error{Code: -32602, Message: fmt.Sprintf("unknown tool: %s", params.Name)},
		}
	}

	result, err := tool.Handler(ctx, params.Arguments)
	if err != nil {
		return Response{
			JSONRPC: "2.0", ID: req.ID,
			Result: map[string]any{
				"content": []map[string]any{
					{"type": "text", "text": fmt.Sprintf("Error: %s", err.Error())},
				},
				"isError": true,
			},
		}
	}

	// Format result as MCP content
	text, _ := json.Marshal(result)
	return Response{
		JSONRPC: "2.0", ID: req.ID,
		Result: map[string]any{
			"content": []map[string]any{
				{"type": "text", "text": string(text)},
			},
		},
	}
}

func (s *Server) writeResponse(resp Response) {
	data, err := json.Marshal(resp)
	if err != nil {
		slog.Error("marshal response", "err", err)
		return
	}
	fmt.Fprintf(s.writer, "%s\n", data)
}
