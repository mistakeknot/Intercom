package container

import (
	"encoding/json"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
)

// Secret key names for each runtime.
var secretKeys = []string{
	// Claude
	"CLAUDE_CODE_OAUTH_TOKEN",
	"ANTHROPIC_API_KEY",
	// Gemini (Code Assist API)
	"GEMINI_REFRESH_TOKEN",
	"GEMINI_OAUTH_CLIENT_ID",
	"GEMINI_OAUTH_CLIENT_SECRET",
	// Codex/OpenAI
	"CODEX_OAUTH_ACCESS_TOKEN",
	"CODEX_OAUTH_REFRESH_TOKEN",
	"CODEX_OAUTH_ID_TOKEN",
	"CODEX_OAUTH_ACCOUNT_ID",
}

// ReadSecrets reads all runtime secrets from .env and Claude OAuth credentials.
//
// For Claude: if neither CLAUDE_CODE_OAUTH_TOKEN nor ANTHROPIC_API_KEY is in
// .env, falls back to reading from ~/.claude/.credentials.json.
func ReadSecrets(projectRoot string) map[string]string {
	envPath := filepath.Join(projectRoot, ".env")
	secrets := readEnvFile(envPath, secretKeys)

	// Auto-refresh: read Claude OAuth from credentials file if not in .env
	if _, ok := secrets["CLAUDE_CODE_OAUTH_TOKEN"]; !ok {
		if _, ok := secrets["ANTHROPIC_API_KEY"]; !ok {
			if token := readClaudeOAuthToken(); token != "" {
				secrets["CLAUDE_CODE_OAUTH_TOKEN"] = token
			}
		}
	}

	return secrets
}

// readEnvFile parses a .env file and returns values for requested keys.
// Does NOT load into process env — callers decide what to do with values.
func readEnvFile(envPath string, keys []string) map[string]string {
	content, err := os.ReadFile(envPath)
	if err != nil {
		slog.Debug(".env file not found", "path", envPath)
		return make(map[string]string)
	}

	wanted := make(map[string]bool, len(keys))
	for _, k := range keys {
		wanted[k] = true
	}

	result := make(map[string]string)
	for _, line := range strings.Split(string(content), "\n") {
		trimmed := strings.TrimSpace(line)
		if trimmed == "" || strings.HasPrefix(trimmed, "#") {
			continue
		}
		eqIdx := strings.IndexByte(trimmed, '=')
		if eqIdx < 0 {
			continue
		}
		key := strings.TrimSpace(trimmed[:eqIdx])
		if !wanted[key] {
			continue
		}
		value := strings.TrimSpace(trimmed[eqIdx+1:])
		// Strip surrounding quotes
		if len(value) >= 2 {
			if (value[0] == '"' && value[len(value)-1] == '"') ||
				(value[0] == '\'' && value[len(value)-1] == '\'') {
				value = value[1 : len(value)-1]
			}
		}
		if value != "" {
			result[key] = value
		}
	}

	return result
}

// readClaudeOAuthToken reads the Claude OAuth token from ~/.claude/.credentials.json.
func readClaudeOAuthToken() string {
	home, err := os.UserHomeDir()
	if err != nil {
		return ""
	}
	credPath := filepath.Join(home, ".claude", ".credentials.json")
	content, err := os.ReadFile(credPath)
	if err != nil {
		return ""
	}
	var data map[string]json.RawMessage
	if err := json.Unmarshal(content, &data); err != nil {
		return ""
	}
	oauthRaw, ok := data["claudeAiOauth"]
	if !ok {
		return ""
	}
	var oauth map[string]string
	if err := json.Unmarshal(oauthRaw, &oauth); err != nil {
		return ""
	}
	token := oauth["accessToken"]
	if token == "" {
		return ""
	}
	slog.Debug("read Claude OAuth token from credentials file")
	return token
}

// BuildContainerArgs constructs Docker CLI args for running a container.
// Produces: docker run -i --rm --pull=never --name {name} -e TZ=... --user ... -v ... {image}
func BuildContainerArgs(mounts []VolumeMount, containerName, image, timezone string) []string {
	return buildContainerArgsInner(mounts, containerName, image, timezone, false)
}

// BuildPoolContainerArgs constructs Docker CLI args for a pool-managed container (no --rm).
func BuildPoolContainerArgs(mounts []VolumeMount, containerName, image, timezone string) []string {
	return buildContainerArgsInner(mounts, containerName, image, timezone, true)
}

func buildContainerArgsInner(mounts []VolumeMount, containerName, image, timezone string, poolManaged bool) []string {
	args := []string{"run", "-i"}
	if !poolManaged {
		args = append(args, "--rm")
	}
	args = append(args, "--pull=never", "--name", containerName)

	// Pass host timezone
	args = append(args, "-e", fmt.Sprintf("TZ=%s", timezone))

	// Run as host user so bind-mounted files are accessible.
	// Skip when running as root (uid 0) or the container's node user (uid 1000).
	uid := os.Getuid()
	gid := os.Getgid()
	if uid != 0 && uid != 1000 {
		args = append(args, "--user", fmt.Sprintf("%d:%d", uid, gid))
		args = append(args, "-e", "HOME=/home/node")
	}

	for _, mount := range mounts {
		if mount.ReadOnly {
			args = append(args, "-v", fmt.Sprintf("%s:%s:ro", mount.HostPath, mount.ContainerPath))
		} else {
			args = append(args, "-v", fmt.Sprintf("%s:%s", mount.HostPath, mount.ContainerPath))
		}

		// Overlay excluded subdirectories with empty tmpfs
		for _, subdir := range mount.Exclude {
			if subdir == "" || strings.Contains(subdir, "..") ||
				strings.Contains(subdir, "/") || strings.Contains(subdir, "\\") ||
				strings.Contains(subdir, ",") {
				slog.Warn("skipping invalid exclude value",
					"subdir", subdir, "host_path", mount.HostPath)
				continue
			}
			args = append(args, "--mount",
				fmt.Sprintf("type=tmpfs,destination=%s/%s,tmpfs-size=0",
					mount.ContainerPath, subdir))
		}
	}

	args = append(args, image)
	return args
}
