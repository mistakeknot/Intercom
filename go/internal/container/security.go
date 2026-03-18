package container

import (
	"encoding/json"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
)

// Default blocked patterns — paths that should never be mounted.
var defaultBlockedPatterns = []string{
	".ssh", ".gnupg", ".gpg", ".aws", ".azure", ".gcloud", ".kube", ".docker",
	"credentials", ".env", ".netrc", ".npmrc", ".pypirc",
	"id_rsa", "id_ed25519", "private_key", ".secret", "/wm",
}

// Paths that are unconditionally blocked regardless of allowlist.
var hardBlockedRoots = []string{"/wm"}

// MountAllowlist is the external allowlist configuration.
type MountAllowlist struct {
	AllowedRoots    []AllowedRoot `json:"allowedRoots"`
	BlockedPatterns []string      `json:"blockedPatterns"`
	NonMainReadOnly bool          `json:"nonMainReadOnly"`
}

// AllowedRoot is a root directory that may be mounted into containers.
type AllowedRoot struct {
	Path           string `json:"path"`
	AllowReadWrite bool   `json:"allowReadWrite"`
	Description    string `json:"description,omitempty"`
}

// ValidatedMount is a mount that passed security validation.
type ValidatedMount struct {
	HostPath      string
	ContainerPath string
	ReadOnly      bool
	Exclude       []string
}

// MountValidationResult holds the outcome of validating a single mount.
type MountValidationResult struct {
	Allowed               bool
	Reason                string
	RealHostPath          string
	ResolvedContainerPath string
	EffectiveReadOnly     *bool
}

// DefaultAllowlistPath returns the default allowlist file location.
func DefaultAllowlistPath() string {
	home, err := os.UserHomeDir()
	if err != nil {
		home = "/root"
	}
	return filepath.Join(home, ".config", "intercom", "mount-allowlist.json")
}

// LoadAllowlist reads the mount allowlist from an external config location.
func LoadAllowlist(path string) *MountAllowlist {
	if _, err := os.Stat(path); os.IsNotExist(err) {
		slog.Warn("mount allowlist not found — additional mounts will be BLOCKED",
			"path", path)
		return nil
	}

	content, err := os.ReadFile(path)
	if err != nil {
		slog.Warn("failed to read mount allowlist — additional mounts will be BLOCKED",
			"path", path, "error", err)
		return nil
	}

	var allowlist MountAllowlist
	if err := json.Unmarshal(content, &allowlist); err != nil {
		slog.Warn("failed to parse mount allowlist — additional mounts will be BLOCKED",
			"path", path, "error", err)
		return nil
	}

	// Merge default blocked patterns with user-configured ones.
	merged := make([]string, len(defaultBlockedPatterns))
	copy(merged, defaultBlockedPatterns)
	for _, p := range allowlist.BlockedPatterns {
		if !contains(merged, p) {
			merged = append(merged, p)
		}
	}
	allowlist.BlockedPatterns = merged

	slog.Info("mount allowlist loaded",
		"path", path,
		"allowed_roots", len(allowlist.AllowedRoots),
		"blocked_patterns", len(allowlist.BlockedPatterns),
	)

	return &allowlist
}

// ValidateMount checks a single additional mount against the allowlist.
func ValidateMount(mount AdditionalMount, isMain bool, allowlist *MountAllowlist) MountValidationResult {
	// Derive container path from host path basename if not specified.
	containerPath := mount.ContainerPath
	if containerPath == "" {
		containerPath = filepath.Base(mount.HostPath)
		if containerPath == "" || containerPath == "." {
			containerPath = "mount"
		}
	}

	if !isValidContainerPath(containerPath) {
		return MountValidationResult{
			Allowed: false,
			Reason: fmt.Sprintf(
				"Invalid container path: %q — must be relative, non-empty, and not contain \"..\"",
				containerPath),
		}
	}

	expanded := expandPath(mount.HostPath)
	if isHardBlocked(expanded) {
		return MountValidationResult{
			Allowed: false,
			Reason:  fmt.Sprintf("Path %q is blocked by hard policy", expanded),
		}
	}

	real, err := filepath.EvalSymlinks(expanded)
	if err != nil {
		return MountValidationResult{
			Allowed: false,
			Reason:  fmt.Sprintf("Host path does not exist: %q (expanded: %q)", mount.HostPath, expanded),
		}
	}
	real, _ = filepath.Abs(real)

	if isHardBlocked(real) {
		return MountValidationResult{
			Allowed: false,
			Reason:  fmt.Sprintf("Path %q is blocked by hard policy", real),
		}
	}

	if pattern := matchesBlockedPattern(real, allowlist.BlockedPatterns); pattern != "" {
		return MountValidationResult{
			Allowed: false,
			Reason:  fmt.Sprintf("Path matches blocked pattern %q: %q", pattern, real),
		}
	}

	allowedRoot := findAllowedRoot(real, allowlist.AllowedRoots)
	if allowedRoot == nil {
		roots := make([]string, len(allowlist.AllowedRoots))
		for i, r := range allowlist.AllowedRoots {
			roots[i] = expandPath(r.Path)
		}
		return MountValidationResult{
			Allowed: false,
			Reason: fmt.Sprintf("Path %q is not under any allowed root. Allowed: %s",
				real, strings.Join(roots, ", ")),
		}
	}

	// Determine effective readonly status.
	requestedRW := !mount.ReadOnly
	effectiveRO := true
	if requestedRW {
		if !isMain && allowlist.NonMainReadOnly {
			slog.Info("mount forced to read-only for non-main group", "mount", mount.HostPath)
			effectiveRO = true
		} else if !allowedRoot.AllowReadWrite {
			slog.Info("mount forced to read-only — root does not allow read-write",
				"mount", mount.HostPath, "root", allowedRoot.Path)
			effectiveRO = true
		} else {
			effectiveRO = false
		}
	}

	reason := fmt.Sprintf("Allowed under root %q", allowedRoot.Path)
	if allowedRoot.Description != "" {
		reason += fmt.Sprintf(" (%s)", allowedRoot.Description)
	}

	return MountValidationResult{
		Allowed:               true,
		Reason:                reason,
		RealHostPath:          real,
		ResolvedContainerPath: containerPath,
		EffectiveReadOnly:     &effectiveRO,
	}
}

// ValidateAdditionalMounts validates all additional mounts for a group.
// Returns only mounts that passed validation.
func ValidateAdditionalMounts(
	mounts []AdditionalMount,
	groupName string,
	isMain bool,
	allowlist *MountAllowlist,
) []ValidatedMount {
	var validated []ValidatedMount

	for _, mount := range mounts {
		result := ValidateMount(mount, isMain, allowlist)
		if result.Allowed {
			validated = append(validated, ValidatedMount{
				HostPath:      result.RealHostPath,
				ContainerPath: "/workspace/extra/" + result.ResolvedContainerPath,
				ReadOnly:      *result.EffectiveReadOnly,
				Exclude:       mount.Exclude,
			})
		} else {
			slog.Warn("additional mount REJECTED",
				"group", groupName,
				"requested_path", mount.HostPath,
				"reason", result.Reason,
			)
		}
	}

	return validated
}

// expandPath expands ~ to home directory.
func expandPath(p string) string {
	home, _ := os.UserHomeDir()
	if home == "" {
		home = "/root"
	}
	if p == "~" {
		return home
	}
	if strings.HasPrefix(p, "~/") {
		return filepath.Join(home, p[2:])
	}
	abs, err := filepath.Abs(p)
	if err != nil {
		return p
	}
	return abs
}

func isHardBlocked(path string) bool {
	for _, root := range hardBlockedRoots {
		if path == root || strings.HasPrefix(path, root+"/") {
			return true
		}
	}
	return false
}

func matchesBlockedPattern(realPath string, patterns []string) string {
	components := strings.Split(realPath, string(filepath.Separator))
	for _, pattern := range patterns {
		for _, comp := range components {
			if comp == pattern || strings.Contains(comp, pattern) {
				return pattern
			}
		}
		if strings.Contains(realPath, pattern) {
			return pattern
		}
	}
	return ""
}

func findAllowedRoot(realPath string, roots []AllowedRoot) *AllowedRoot {
	for i := range roots {
		expanded := expandPath(roots[i].Path)
		realRoot, err := filepath.EvalSymlinks(expanded)
		if err != nil {
			continue
		}
		realRoot, _ = filepath.Abs(realRoot)
		rel, err := filepath.Rel(realRoot, realPath)
		if err != nil {
			continue
		}
		// filepath.Rel returns paths starting with ".." when not under root
		if !strings.HasPrefix(rel, "..") {
			return &roots[i]
		}
	}
	return nil
}

func isValidContainerPath(p string) bool {
	return p != "" && !strings.Contains(p, "..") && !strings.HasPrefix(p, "/")
}

func contains(ss []string, s string) bool {
	for _, v := range ss {
		if v == s {
			return true
		}
	}
	return false
}
