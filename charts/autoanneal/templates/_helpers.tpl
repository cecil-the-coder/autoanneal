{{/*
Fullname: release-name truncated to 63 chars.
*/}}
{{- define "autoanneal.fullname" -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Chart label value.
*/}}
{{- define "autoanneal.chart" -}}
{{ .Chart.Name }}-{{ .Chart.Version | replace "+" "_" }}
{{- end }}

{{/*
Common labels.
*/}}
{{- define "autoanneal.labels" -}}
helm.sh/chart: {{ include "autoanneal.chart" . }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{ include "autoanneal.selectorLabels" . }}
{{- end }}

{{/*
Selector labels.
*/}}
{{- define "autoanneal.selectorLabels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Secret name: either existingSecret or the chart-created one.
*/}}
{{- define "autoanneal.secretName" -}}
{{- if .Values.secrets.existingSecret -}}
{{ .Values.secrets.existingSecret }}
{{- else -}}
{{ include "autoanneal.fullname" . }}-secrets
{{- end -}}
{{- end }}

{{/*
Build CLI args for a repo entry.
Expects a dict with keys: repo (the repo entry), root (top-level context).
*/}}
{{- define "autoanneal.args" -}}
{{- $repo := .repo -}}
{{- $defaults := .root.Values.defaults -}}
- {{ $repo.repo | quote }}
- "--max-budget"
- {{ ($repo.maxBudget | default $defaults.maxBudget) | quote }}
- "--timeout"
- {{ ($repo.timeout | default $defaults.timeout) | quote }}
- "--model"
- {{ ($repo.model | default $defaults.model) | quote }}
- "--max-tasks"
- {{ ($repo.maxTasks | default $defaults.maxTasks) | quote }}
- "--min-severity"
- {{ ($repo.minSeverity | default $defaults.minSeverity) | quote }}
- "--log-level"
- {{ ($repo.logLevel | default $defaults.logLevel) | quote }}
{{- if or $repo.dryRun $defaults.dryRun }}
- "--dry-run"
{{- end }}
{{- if or $repo.setupCommand $defaults.setupCommand }}
- "--setup-command"
- {{ ($repo.setupCommand | default $defaults.setupCommand) | quote }}
{{- end }}
- "--skip-after"
- {{ ($repo.skipAfter | default $defaults.skipAfter) | quote }}
- "--cron-interval"
- {{ ($repo.cronInterval | default $defaults.cronInterval) | quote }}
- "--fix-ci"
- {{ (ternary $repo.fixCi $defaults.fixCi (hasKey $repo "fixCi")) | quote }}
- "--fix-conflicts"
- {{ (ternary $repo.fixConflicts $defaults.fixConflicts (hasKey $repo "fixConflicts")) | quote }}
- "--critic-threshold"
- {{ ($repo.criticThreshold | default $defaults.criticThreshold) | quote }}
- "--improve-docs"
- {{ (ternary $repo.improveDocs $defaults.improveDocs (hasKey $repo "improveDocs")) | quote }}
- "--doc-critic-threshold"
- {{ ($repo.docCriticThreshold | default $defaults.docCriticThreshold) | quote }}
- "--review-prs"
- {{ (ternary $repo.reviewPrs $defaults.reviewPrs (hasKey $repo "reviewPrs")) | quote }}
- "--review-filter"
- {{ ($repo.reviewFilter | default $defaults.reviewFilter) | quote }}
- "--review-fix-threshold"
- {{ ($repo.reviewFixThreshold | default $defaults.reviewFixThreshold) | quote }}
- "--concurrency"
- {{ ($repo.concurrency | default $defaults.concurrency) | quote }}
{{- if or $repo.investigateIssues $defaults.investigateIssues }}
- "--investigate-issues"
- {{ ($repo.investigateIssues | default $defaults.investigateIssues) | quote }}
{{- end }}
- "--max-issues"
- {{ ($repo.maxIssues | default $defaults.maxIssues) | quote }}
- "--issue-budget"
- {{ ($repo.issueBudget | default $defaults.issueBudget) | quote }}
- "--max-open-prs"
- {{ ($repo.maxOpenPrs | default $defaults.maxOpenPrs) | quote }}
{{- end }}

{{/*
Container spec for a repo entry.
Expects a dict with keys: repo (the repo entry), root (top-level context).
*/}}
{{- define "autoanneal.container" -}}
{{- $root := .root -}}
name: autoanneal
image: "{{ $root.Values.image.repository }}:{{ $root.Values.image.tag }}"
imagePullPolicy: {{ $root.Values.image.pullPolicy }}
args:
  {{- include "autoanneal.args" . | nindent 2 }}
envFrom:
  - secretRef:
      name: {{ include "autoanneal.secretName" $root }}
  - configMapRef:
      name: {{ include "autoanneal.fullname" $root }}-config
      optional: true
{{- with $root.Values.containerSecurityContext }}
securityContext:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with $root.Values.resources }}
resources:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- end }}

{{/*
Pod spec shared between CronJob and Job.
Expects a dict with keys: repo (the repo entry), root (top-level context).
*/}}
{{- define "autoanneal.podSpec" -}}
{{- $root := .root -}}
restartPolicy: Never
{{- with $root.Values.imagePullSecrets }}
imagePullSecrets:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with $root.Values.podSecurityContext }}
securityContext:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with $root.Values.nodeSelector }}
nodeSelector:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with $root.Values.tolerations }}
tolerations:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with $root.Values.affinity }}
affinity:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- if and $root.Values.serviceAccount.create $root.Values.serviceAccount.name }}
serviceAccountName: {{ $root.Values.serviceAccount.name }}
{{- end }}
containers:
  - {{- include "autoanneal.container" . | nindent 4 }}
{{- end }}
