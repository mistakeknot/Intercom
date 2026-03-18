package container

import (
	"encoding/json"
	"fmt"
	"io/fs"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
	"time"
)

// GroupInfo holds registered group information needed for mount building.
type GroupInfo struct {
	Folder          string
	Name            string
	ContainerConfig *ContainerConfig
}

// ContainerConfig holds per-group container settings from registration.
type ContainerConfig struct {
	AdditionalMounts []AdditionalMount `json:"additionalMounts"`
	Timeout          *uint64           `json:"timeout,omitempty"`
}

// AdditionalMount is a user-requested extra mount from group configuration.
type AdditionalMount struct {
	HostPath      string   `json:"hostPath"`
	ContainerPath string   `json:"containerPath,omitempty"`
	ReadOnly      bool     `json:"readonly"`
	Exclude       []string `json:"exclude,omitempty"`
}

// BuildVolumeMounts constructs the mount list for a container invocation.
//
// Mount structure:
//   - Main: project root (ro) + group folder (rw) + global (if exists)
//   - Non-main: group folder (rw) + global (ro)
//   - Claude: per-group .claude/ sessions directory
//   - All: per-group IPC namespace, runner source (ro), shared source (non-Claude)
//   - Additional mounts from group config (validated against allowlist)
func BuildVolumeMounts(
	group *GroupInfo,
	isMain bool,
	runtime RuntimeKind,
	projectRoot, groupsDir, dataDir string,
	allowlist *MountAllowlist,
) []VolumeMount {
	var mounts []VolumeMount
	groupDir := filepath.Join(groupsDir, group.Folder)

	if isMain {
		// Main gets the project root read-only.
		mounts = append(mounts, VolumeMount{
			HostPath:      projectRoot,
			ContainerPath: "/workspace/project",
			ReadOnly:      true,
		})

		// Main also gets its group folder as the working directory.
		os.MkdirAll(groupDir, 0o755)
		mounts = append(mounts, VolumeMount{
			HostPath:      groupDir,
			ContainerPath: "/workspace/group",
			ReadOnly:      false,
		})
	} else {
		// Other groups only get their own folder.
		os.MkdirAll(groupDir, 0o755)
		mounts = append(mounts, VolumeMount{
			HostPath:      groupDir,
			ContainerPath: "/workspace/group",
			ReadOnly:      false,
		})

		// Global memory directory (read-only for non-main).
		globalDir := filepath.Join(groupsDir, "global")
		if info, err := os.Stat(globalDir); err == nil && info.IsDir() {
			mounts = append(mounts, VolumeMount{
				HostPath:      globalDir,
				ContainerPath: "/workspace/global",
				ReadOnly:      true,
			})
		}
	}

	// Claude runtime: per-group .claude/ sessions directory.
	if runtime == RuntimeClaude {
		sessionsDir := filepath.Join(dataDir, "sessions", group.Folder, ".claude")
		os.MkdirAll(sessionsDir, 0o755)

		// Create default settings if missing.
		settingsFile := filepath.Join(sessionsDir, "settings.json")
		if _, err := os.Stat(settingsFile); os.IsNotExist(err) {
			defaultSettings := map[string]any{
				"env": map[string]string{
					"CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS":         "1",
					"CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD": "1",
					"CLAUDE_CODE_DISABLE_AUTO_MEMORY":              "0",
				},
			}
			if data, err := json.MarshalIndent(defaultSettings, "", "  "); err == nil {
				os.WriteFile(settingsFile, append(data, '\n'), 0o644)
			}
		}

		// Sync skills from container/skills/ into sessions.
		skillsSrc := filepath.Join(projectRoot, "container", "skills")
		if info, err := os.Stat(skillsSrc); err == nil && info.IsDir() {
			skillsDst := filepath.Join(sessionsDir, "skills")
			copyDirRecursive(skillsSrc, skillsDst)
		}

		mounts = append(mounts, VolumeMount{
			HostPath:      sessionsDir,
			ContainerPath: "/home/node/.claude",
			ReadOnly:      false,
		})
	}

	// Per-group IPC namespace.
	ipcDir := filepath.Join(dataDir, "ipc", group.Folder)
	for _, sub := range []string{"messages", "tasks", "input", "queries", "responses"} {
		os.MkdirAll(filepath.Join(ipcDir, sub), 0o755)
	}
	mounts = append(mounts, VolumeMount{
		HostPath:      ipcDir,
		ContainerPath: "/workspace/ipc",
		ReadOnly:      false,
	})

	// Mount agent-runner source from host (recompiled on container startup).
	runnerSrc := filepath.Join(projectRoot, "container", RunnerDirName(runtime), "src")
	if info, err := os.Stat(runnerSrc); err == nil && info.IsDir() {
		mounts = append(mounts, VolumeMount{
			HostPath:      runnerSrc,
			ContainerPath: RunnerContainerPath(runtime),
			ReadOnly:      true,
		})
	}

	// All runtimes need the shared code mounted for live recompilation.
	sharedSrc := filepath.Join(projectRoot, "container", "shared")
	if info, err := os.Stat(sharedSrc); err == nil && info.IsDir() {
		mounts = append(mounts, VolumeMount{
			HostPath:      sharedSrc,
			ContainerPath: "/app/shared",
			ReadOnly:      true,
		})
	}

	// Additional mounts validated against external allowlist.
	if group.ContainerConfig != nil && len(group.ContainerConfig.AdditionalMounts) > 0 {
		if allowlist != nil {
			validated := ValidateAdditionalMounts(
				group.ContainerConfig.AdditionalMounts,
				group.Name,
				isMain,
				allowlist,
			)
			for _, vm := range validated {
				mounts = append(mounts, VolumeMount{
					HostPath:      vm.HostPath,
					ContainerPath: vm.ContainerPath,
					ReadOnly:      vm.ReadOnly,
					Exclude:       vm.Exclude,
				})
			}
		} else {
			slog.Warn("skipping additional mounts — no allowlist loaded",
				"group", group.Name,
				"count", len(group.ContainerConfig.AdditionalMounts),
			)
		}
	}

	return mounts
}

// ContainerName generates a safe container name from group folder and timestamp.
func ContainerName(groupFolder string) string {
	var safe strings.Builder
	for _, c := range groupFolder {
		if (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '-' {
			safe.WriteRune(c)
		} else {
			safe.WriteByte('-')
		}
	}
	return fmt.Sprintf("intercom-%s-%d", safe.String(), time.Now().UnixMilli())
}

// copyDirRecursive copies src directory contents into dst.
func copyDirRecursive(src, dst string) {
	os.MkdirAll(dst, 0o755)
	entries, err := os.ReadDir(src)
	if err != nil {
		return
	}
	for _, entry := range entries {
		srcPath := filepath.Join(src, entry.Name())
		dstPath := filepath.Join(dst, entry.Name())
		if entry.IsDir() {
			copyDirRecursive(srcPath, dstPath)
		} else {
			copyFile(srcPath, dstPath)
		}
	}
}

func copyFile(src, dst string) {
	data, err := os.ReadFile(src)
	if err != nil {
		return
	}
	info, _ := os.Stat(src)
	var perm fs.FileMode = 0o644
	if info != nil {
		perm = info.Mode().Perm()
	}
	os.WriteFile(dst, data, perm)
}
