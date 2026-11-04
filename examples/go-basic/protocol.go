// Code generated from JSON Schema using quicktype. DO NOT EDIT.
// To parse and unparse this JSON data, add this code to your project and do:
//
//    protocol, err := UnmarshalProtocol(bytes)
//    bytes, err = protocol.Marshal()

package main

import "bytes"
import "errors"
import "time"

import "encoding/json"

func UnmarshalProtocol(data []byte) (Protocol, error) {
	var r Protocol
	err := json.Unmarshal(data, &r)
	return r, err
}

func (r *Protocol) Marshal() ([]byte, error) {
	return json.Marshal(r)
}

// One entry point per wire surface; every protocol type is named under $defs.
type Protocol struct {
	ClientInput         *ClientInput         `json:"client_input,omitempty"`
	ClientPayload       *ClientPayload       `json:"client_payload,omitempty"`
	DecisionRequest     *DecisionRequest     `json:"decision_request,omitempty"`
	DecisionResponse    *DecisionResponse    `json:"decision_response,omitempty"`
	InterruptPayload    *InterruptPayload    `json:"interrupt_payload,omitempty"`
	InterruptResolution *InterruptResolution `json:"interrupt_resolution,omitempty"`
	StreamDelta         *StreamDelta         `json:"stream_delta,omitempty"`
	TokenDelta          *TokenDelta          `json:"token_delta,omitempty"`
}

// Everything a client can send: submit a message, a full view, an append
// batch, or a named action; resume an interrupt; or settle a client tool.
//
// Each variant carries only the addressing it needs. The four submit variants
// carry `agent_id`, which routes the turn and starts the session if it is new,
// and an optional `turn_id`. A resume or settle names an interrupt or effect
// and continues whatever turn is running, so it carries neither.
// `session_id` is on the envelope.
//
// The body of an interrupt resume: which interrupt, and the payload delivered
// to the worker. Shared by the [`ClientInput::InterruptResume`] input and the
// [`DecisionTrigger::InterruptResumed`] trigger.
type ClientInput struct {
	AgentID           *string         `json:"agent_id,omitempty"`
	Message           *DraftMessage   `json:"message,omitempty"`
	Queue             *Queue          `json:"queue"`
	Stream            *bool           `json:"stream,omitempty"`
	TurnID            *string         `json:"turn_id"`
	Type              ClientInputType `json:"type"`
	Client            *ClientContext  `json:"client,omitempty"`
	Messages          []DraftMessage  `json:"messages,omitempty"`
	Args              interface{}     `json:"args"`
	Name              *string         `json:"name,omitempty"`
	InterruptID       *string         `json:"interrupt_id,omitempty"`
	Payload           interface{}     `json:"payload"`
	Attempt           *int64          `json:"attempt"`
	Content           []ToolContent   `json:"content"`
	ID                *string         `json:"id,omitempty"`
	IsError           *bool           `json:"is_error,omitempty"`
	Result            interface{}     `json:"result"`
	StructuredContent interface{}     `json:"structured_content"`
	Error             *string         `json:"error,omitempty"`
	Retryable         *bool           `json:"retryable,omitempty"`
}

// Inputs a client declares on its run, passed to the worker on the
// `client.messages` decision. `tools` are the browser's own tools, read as
// client-handled [`AgentTool`]s. The engine adds them to the proposed config.
// A worker can override that by returning its own `agent`.
//
// Inputs the client declared on its run. The engine adds
// `client.tools` to the proposed config.
type ClientContext struct {
	Context        []interface{} `json:"context,omitempty"`
	ForwardedProps interface{}   `json:"forwarded_props"`
	State          interface{}   `json:"state"`
	Tools          []AgentTool   `json:"tools,omitempty"`
}

// A function tool the agent offers. `handler` says where a call runs:
// `client` for the client, absent for the worker. `server` is invalid here.
// The engine runs only connector tools, and a worker declares those by
// connection id.
type AgentTool struct {
	// Keep this tool out of the request. See [`LlmTool::defer`]. Absent ⇒
	// the agent's `defer_tools`.
	Defer       *bool       `json:"defer"`
	Description *string     `json:"description,omitempty"`
	Handler     *Handler    `json:"handler"`
	Input       interface{} `json:"input"`
	Name        string      `json:"name"`
	Output      interface{} `json:"output"`
}

type ToolContent struct {
	Text     *string           `json:"text,omitempty"`
	Type     ToolContentType   `json:"type"`
	Data     *string           `json:"data,omitempty"`
	MIMEType *string           `json:"mimeType"`
	Resource *ResourceContents `json:"resource,omitempty"`
	Name     *string           `json:"name"`
	URI      *string           `json:"uri,omitempty"`
}

type ResourceContents struct {
	Blob     *string `json:"blob"`
	MIMEType *string `json:"mimeType"`
	Text     *string `json:"text"`
	URI      string  `json:"uri"`
}

// The wire form of a [`Message`]. `id` is absent until the message is
// recorded.
type DraftMessage struct {
	Content    *Content          `json:"content"`
	ID         *string           `json:"id"`
	Name       *string           `json:"name"`
	Reasoning  *Reasoning        `json:"reasoning"`
	Role       Role              `json:"role"`
	ToolCallID *string           `json:"tool_call_id"`
	ToolCalls  []ToolCallElement `json:"tool_calls"`
}

type ContentPartWire struct {
	Text       *string              `json:"text,omitempty"`
	Type       *ContentPartWireType `json:"type,omitempty"`
	URI        *string              `json:"uri,omitempty"`
	MIMEType   *string              `json:"mimeType"`
	Name       *string              `json:"name"`
	ImageURL   *ImageURLClass       `json:"image_url,omitempty"`
	File       *FileData            `json:"file,omitempty"`
	InputAudio *AudioData           `json:"input_audio,omitempty"`
	VideoURL   *VideoURLClass       `json:"video_url,omitempty"`
}

type FileData struct {
	FileData string `json:"file_data"`
	Filename string `json:"filename"`
}

type ImageURLClass struct {
	URL string `json:"url"`
}

type AudioData struct {
	Data   string `json:"data"`
	Format string `json:"format"`
}

type VideoURLClass struct {
	URL string `json:"url"`
}

// What the model thought before it answered. `text` is for a reader.
// `blocks` are the provider's own and stay unchanged. Anthropic requires the
// thinking before a tool call back with its signature.
type Reasoning struct {
	Blocks   []interface{}     `json:"blocks,omitempty"`
	Provider ReasoningProvider `json:"provider"`
	Text     *string           `json:"text"`
}

type ToolCallElement struct {
	Function ToolCallFunction `json:"function"`
	ID       string           `json:"id"`
	Type     string           `json:"type"`
}

type ToolCallFunction struct {
	Arguments string `json:"arguments"`
	Name      string `json:"name"`
}

// What a client submits: a message, its full conversation view, an append
// batch, or a named action. The engine turns it into events and never stores
// it as it arrived.
//
// The body of a `client.message`: one message, optionally streamed.
//
// The body of a `client.messages`: the client's full conversation view, optionally
// streamed.
//
// The body of a `client.append`. Messages are added at the session head,
// against the active path at delivery. A queued append lands after whatever
// turn beat it, so it cannot fork the tree. A message whose id is already
// recorded is dropped.
//
// The payload of a `client.action`: a named action with optional JSON args.
type ClientPayload struct {
	Message  *DraftMessage     `json:"message,omitempty"`
	Stream   *bool             `json:"stream,omitempty"`
	Type     ClientPayloadType `json:"type"`
	Client   *ClientContext    `json:"client,omitempty"`
	Messages []DraftMessage    `json:"messages,omitempty"`
	Args     interface{}       `json:"args"`
	Name     *string           `json:"name,omitempty"`
}

type DecisionRequest struct {
	// The agent config for the active path. `null` when none is set.
	Agent      *AgentConfig   `json:"agent"`
	AgentID    string         `json:"agent_id"`
	Ancestry   []string       `json:"ancestry"`
	Attempts   int64          `json:"attempts"`
	Calls      []Effect       `json:"calls"`
	Deadline   *time.Time     `json:"deadline"`
	DecisionID string         `json:"decision_id"`
	Identity   WorkerIdentity `json:"identity"`
	// A message waits to join the turn. Its decision calls the model next,
	// so do not call the model or end the turn.
	Joining  *bool     `json:"joining,omitempty"`
	Messages []Message `json:"messages"`
	// Count of in-flight `tool_call`/`subagent` calls.
	PendingCalls int64 `json:"pending_calls"`
	// The engine's default continuation for `trigger`. Empty when only the
	// worker can decide. Accept it by echoing it back.
	Proposed  DecisionResponse `json:"proposed"`
	SessionID string           `json:"session_id"`
	State     interface{}      `json:"state"`
	Trigger   DecisionTrigger  `json:"trigger"`
	// The turn this decision belongs to. `null` outside a turn.
	Turn *TurnClass `json:"turn"`
	// The worker this session is pinned to. `null` when the file's own
	// routing decides.
	Worker *WorkerRef `json:"worker"`
}

// A declared agent. The same shape whether a file writes it or a worker
// returns it.
//
// `llm` names the `[llm.*]` block every call runs on. That block decides where
// the call runs and what shape it takes. A config that names no block fails
// when the engine resolves a call.
type AgentConfig struct {
	Attachments *Attachments `json:"attachments"`
	// Offer `run_code`: the model writes a script in this language that
	// calls the agent's tools.
	Codemode *CodemodeEnum  `json:"codemode"`
	Context  *ContextConfig `json:"context"`
	// Defer every tool this agent offers, whatever its source. A tool or a
	// connection overrides this with its own `defer`. Absent, the agent defers
	// nothing; a connection can still defer on its own.
	DeferTools *DeferToolsWire `json:"defer_tools"`
	// How hard the model thinks. Unset leaves the provider's own default.
	Effort *ReasoningEffort `json:"effort"`
	// The `[llm.*]` block this agent's calls run on.
	Llm *string `json:"llm"`
	// How deep this agent's subagents may nest. A session whose depth
	// reaches it may not delegate. `0` never delegates.
	MaxSubagentDepth *int64 `json:"max_subagent_depth"`
	// MCP servers this agent draws tools from.
	MCP []MCPServer `json:"mcp,omitempty"`
	// Whether the engine tells the model that an MCP server is available, and
	// what that server says it is for.
	MCPAnnounce *MCPAnnounce `json:"mcp_announce,omitempty"`
	Model       string       `json:"model"`
	// Plugins this agent uses.
	Plugins []AgentPlugin `json:"plugins,omitempty"`
	// Boxed. Five per-kind overrides are too many bytes to carry inline.
	Retry *RetryConfig `json:"retry"`
	// Skills of its own, outside any plugin.
	Skills []AgentSkill `json:"skills,omitempty"`
	// What shape the subagents take as tools. Absent ⇒ one tool per agent.
	SubagentTools *SubagentTools `json:"subagent_tools"`
	// Subagents the model can delegate to. The model sees them as tools.
	// Each call starts a child session.
	Subagents []SubagentElement `json:"subagents,omitempty"`
	System    *string           `json:"system"`
	// Worker- or client-executed tools the model can call.
	Tools []AgentTool `json:"tools,omitempty"`
}

type Attachments struct {
	MaxInline *Size                  `json:"max_inline"`
	Rules     map[string]Disposition `json:"rules,omitempty"`
	Tools     []AttachmentTool       `json:"tools,omitempty"`
}

// How the agent makes room in what the model reads. Before each model call
// the engine tries the strategies in order. The first that applies proposes
// a plan on a `context.limit` decision, and the worker has the final say.
type ContextConfig struct {
	Strategy []ContextStrategy `json:"strategy,omitempty"`
	// The model's context window in tokens. Without it, only `idle_secs`,
	// `over_tokens` and a provider's refusal start making room.
	Window *int64 `json:"window"`
}

// One way to make room, and when it applies.
type ContextStrategy struct {
	// At a turn's first model call, after this many seconds with no turn.
	IdleSecs *int64 `json:"idle_secs"`
	// `summarize`: about how many recent tokens are kept as they are, from a
	// user message.
	KeepTokens *int64 `json:"keep_tokens"`
	// `summarize`: the `[llm.*]` block the summary runs on.
	Llm *string `json:"llm"`
	// `summarize`: the model that writes the summary.
	Model *string `json:"model"`
	// When the model would read more than this. Without `idle_secs`, it
	// defaults to the window less 16000.
	OverTokens *int64 `json:"over_tokens"`
	// `summarize`: the instructions for the summary, in place of the engine's.
	Prompt *string             `json:"prompt"`
	Use    ContextStrategyKind `json:"use"`
}

// How an agent's deferred tools reach the model.
type DeferTools struct {
	// The most matches one search answers with. Never zero: a search that can
	// answer with nothing is a search the model cannot use.
	MaxMatches *int64 `json:"max_matches,omitempty"`
	// How the model learns the agent's skills.
	Skills *SkillListing `json:"skills,omitempty"`
	// Which tools the agent gets to reach the ones it defers.
	Strategy *DeferToolsStrategy `json:"strategy,omitempty"`
}

// An MCP server the agent draws tools from. `id` names an `[mcp.*]`
// connection the engine holds. A worker never writes a URL or a credential.
type MCPServer struct {
	Approve         *Approve            `json:"approve,omitempty"`
	AuthFailure     *MCPAuthFailure     `json:"auth_failure,omitempty"`
	ID              string              `json:"id"`
	ToolSyncFailure *MCPToolSyncFailure `json:"tool_sync_failure,omitempty"`
	// Narrows what the model sees. Absent ⇒ every tool the connection grants.
	Tools *MCPTools `json:"tools"`
}

// Which of a connection's tools the model sees, and how they reach it.
//
// The filter runs in order: capability predicates, then `include`, then
// `exclude`. Each step only removes, so a filter cannot widen what the
// connection grants. `defer` runs last and removes nothing.
//
// `include` and `exclude` are globs over the tool's name on the connection,
// not the prefixed name the model sees.
//
// Capability predicates read the MCP annotations. A tool with no annotation
// fails the predicate, so a server that annotates nothing yields nothing under
// `read_only`.
type MCPTools struct {
	// Keep every surviving tool out of the request. See [`LlmTool::defer`].
	// Absent ⇒ the agent's `defer_tools`.
	Defer          *bool    `json:"defer"`
	Exclude        []string `json:"exclude,omitempty"`
	Idempotent     *bool    `json:"idempotent"`
	Include        []string `json:"include,omitempty"`
	NonDestructive *bool    `json:"non_destructive"`
	ReadOnly       *bool    `json:"read_only"`
}

// A plugin an agent uses. The skills and servers come from the bundle when the
// config loads.
type AgentPlugin struct {
	Approve     *Approve        `json:"approve,omitempty"`
	AuthFailure *MCPAuthFailure `json:"auth_failure,omitempty"`
	Description *string         `json:"description,omitempty"`
	ID          string          `json:"id"`
	// The plugin's server names, from its bundle.
	Servers []string    `json:"servers,omitempty"`
	Skills  []SkillMeta `json:"skills,omitempty"`
	// Where its skills come from. Absent ⇒ its bundle.
	Source          *SkillSource        `json:"source,omitempty"`
	ToolSyncFailure *MCPToolSyncFailure `json:"tool_sync_failure,omitempty"`
	// Applied to each of the plugin's servers.
	Tools *MCPTools `json:"tools"`
}

// What the model sees of a skill before it loads it.
type SkillMeta struct {
	Description *string `json:"description,omitempty"`
	Name        string  `json:"name"`
	// Tools the model can't see or call until it loads this skill. A tool
	// that several skills name comes with any of them.
	Tools []string `json:"tools,omitempty"`
}

// Retry overrides, one for each effect kind. `default` covers the kinds that
// name nothing. A kind layers on top of `default`.
type RetryConfig struct {
	Connector *RetryOverride `json:"connector"`
	Default   *RetryOverride `json:"default"`
	Llm       *RetryOverride `json:"llm"`
	Subagent  *RetryOverride `json:"subagent"`
	System1   *RetryOverride `json:"system1"`
	Tool      *RetryOverride `json:"tool"`
}

// Only the fields it names change. An override cannot make a timeout
// unbounded.
type RetryOverride struct {
	BackoffBaseSecs  *int64 `json:"backoff_base_secs"`
	BackoffMaxSecs   *int64 `json:"backoff_max_secs"`
	MaxAttempts      *int64 `json:"max_attempts"`
	QueueTimeoutSecs *int64 `json:"queue_timeout_secs"`
	RunTimeoutSecs   *int64 `json:"run_timeout_secs"`
	TotalTimeoutSecs *int64 `json:"total_timeout_secs"`
}

// A skill of the agent's own, in no plugin. The model loads it by its bare
// name.
type AgentSkill struct {
	Description *string `json:"description,omitempty"`
	Name        string  `json:"name"`
	// Absent ⇒ the project's skills bundle, which holds every `[skill.<id>]`
	// with a `path`.
	Source *SkillSource `json:"source,omitempty"`
	// See [`SkillMeta::tools`].
	Tools []string `json:"tools,omitempty"`
}

// What shape an agent's subagents take as tools.
type SubagentTools struct {
	Strategy *SubagentToolsStrategy `json:"strategy,omitempty"`
	Wait     *bool                  `json:"wait"`
}

// A subagent the model can delegate to. `id` names the child agent; the model
// calls the tool [`Subagent::offered_name`] gives. Its input is one `message`.
type SubagentElement struct {
	// Keep this tool out of the request. See [`LlmTool::defer`]. Absent ⇒
	// the agent's `defer_tools`.
	Defer       *bool         `json:"defer"`
	Description *string       `json:"description,omitempty"`
	ID          string        `json:"id"`
	Mode        *SubagentMode `json:"mode"`
	// Offer the tool as `agent__<id>` instead of `<id>`.
	Prefix *bool `json:"prefix"`
}

// An effect still running, shown on each worker decision. A flat envelope
// plus the fields of its kind. A connector sync carries none. Its `id` is the
// connection being fetched.
type Effect struct {
	AgentID *string `json:"agent_id"`
	// The tree node the effect was requested at.
	Anchor    *string    `json:"anchor"`
	Arguments *string    `json:"arguments"`
	Attempt   int64      `json:"attempt"`
	Deadline  *time.Time `json:"deadline"`
	Handler   *Handler   `json:"handler"`
	ID        string     `json:"id"`
	Kind      EffectKind `json:"kind"`
	Name      *string    `json:"name"`
	// The child session a subagent runs in. Its `id` is the model tool call
	// the subagent answers.
	SessionID *string      `json:"session_id"`
	Status    EffectStatus `json:"status"`
	Stream    *bool        `json:"stream"`
}

// The owner as the worker receives it, without the tenant. Read `kind` with
// `id`. Only `frontend` is an end user.
type WorkerIdentity struct {
	Metadata   map[string]string `json:"metadata"`
	Subject    *Subject          `json:"subject"`
	Visibility *Visibility       `json:"visibility,omitempty"`
}

// One identity, as the source that authenticated it named it. An id is
// unique only within its issuer.
type Subject struct {
	ID     string `json:"id"`
	Issuer string `json:"issuer"`
}

type Message struct {
	Content    *Content          `json:"content"`
	ID         string            `json:"id"`
	Name       *string           `json:"name"`
	Reasoning  *Reasoning        `json:"reasoning"`
	Role       Role              `json:"role"`
	ToolCallID *string           `json:"tool_call_id"`
	ToolCalls  []ToolCallElement `json:"tool_calls,omitempty"`
}

// The engine's default continuation for `trigger`. Empty when only the
// worker can decide. Accept it by echoing it back.
//
// The messages and actions to author, plus optional state and agent writes.
// A worker returns one. The engine proposes one too, which the worker echoes
// or changes.
type DecisionResponse struct {
	Actions []DecisionAction `json:"actions,omitempty"`
	// A new agent config write; omitted keeps the current config.
	Agent    *AgentConfig   `json:"agent"`
	Messages []DraftMessage `json:"messages,omitempty"`
	// Absent or `null` keeps the current state. Send an empty value to
	// clear it.
	State interface{} `json:"state"`
}

// The action a worker writes on the wire. A settle can leave out the effect
// id, because the `*.execute` trigger it answers already names it.
//
// A flat LLM request. Every field is optional.
//
// Without `id`, the engine mints one, and it becomes the assistant node's
// id. A field left out comes from the agent config, then from the engine's
// default. Without `messages`, the request carries the config's system
// message and the decision's view. Given `messages`, no system message is
// added.
//
// Without `id`, the engine mints one. A tool the model called carries the
// model's id.
//
// There is no `handler`. The name says where the call runs: a connector
// tool on the engine, a `handler: client` tool on the client, anything
// else on the worker. The engine knows all three already.
//
// Without `id` and `attempt`, both come from the `tool.execute` trigger
// this answers. That ties the result to the attempt that ran.
//
// Without `id` and `attempt`, both come from the `llm.execute` trigger
// this answers. That ties the result to the attempt that ran.
//
// Ask a `[system1.*]` block typed questions about `state`. The answers
// come back as `system1.finished`.
//
// Without `id`, the engine mints one. Without `system1`, the call runs on
// the only block declared. Without `model`, it uses the block's. Without
// `state`, the model reads the conversation.
//
// Without `interrupt_id`, the engine mints one to match the later
// resume.
//
// Resolve an open interrupt and resume the session.
//
// Fetch a connection's tools again, after a person replaced its
// credential.
//
// End the turn as a failed run.
//
// Set what the model reads, on a new branch. `summary` comes first, as a
// message saying the conversation before it was summarized. A message
// whose `id` is recorded is kept as a copy; any other is written new.
// The worker's state and the config carry over.
type DecisionAction struct {
	ID *string `json:"id"`
	// The `[llm.*]` block this call runs on. Absent uses the config's
	// `llm`. Naming another block moves this one call elsewhere.
	Llm                 *string          `json:"llm"`
	MaxCompletionTokens *int64           `json:"max_completion_tokens"`
	Messages            []DraftMessage   `json:"messages"`
	Model               *string          `json:"model"`
	Reasoning           *ReasoningConfig `json:"reasoning"`
	// Layered over the agent config's `llm` policy, or over the engine's
	// default.
	//
	// Layered over the agent config's policy for this kind, or over the
	// engine's default for where the tool runs.
	//
	// Layered over the agent config's `subagent` policy, or over the
	// engine's default.
	//
	// Layered over the agent config's `system1` policy, or over the
	// engine's default.
	Retry             *RetryOverride     `json:"retry"`
	Stream            *bool              `json:"stream"`
	Temperature       *float64           `json:"temperature"`
	Tools             []LlmTool          `json:"tools"`
	Type              DecisionActionType `json:"type"`
	Arguments         interface{}        `json:"arguments"`
	Name              *string            `json:"name,omitempty"`
	Attempt           *int64             `json:"attempt"`
	Content           []ToolContent      `json:"content"`
	IsError           *bool              `json:"is_error,omitempty"`
	Result            interface{}        `json:"result"`
	StructuredContent interface{}        `json:"structured_content"`
	// An `LlmResponse`, or the provider's own response when the
	// `llm.execute` this answers carried a `format`.
	Response interface{} `json:"response"`
	Code     *ErrorCode  `json:"code"`
	Detail   interface{} `json:"detail"`
	Error    *Error      `json:"error"`
	// Omitted ⇒ terminal.
	Retryable *bool   `json:"retryable,omitempty"`
	AgentID   *string `json:"agent_id,omitempty"`
	// The child's opening message. It travels with the spawn, so it
	// cannot arrive before the session exists.
	Message   *DraftMessage `json:"message"`
	Mode      *SpawnMode    `json:"mode"`
	SessionID *string       `json:"session_id"`
	// The model tool call this subagent answers. Required.
	ToolCallID  *string             `json:"tool_call_id,omitempty"`
	Assets      map[string]Asset    `json:"assets,omitempty"`
	Questions   map[string]Question `json:"questions,omitempty"`
	State       interface{}         `json:"state"`
	System1     *string             `json:"system1"`
	InterruptID *string             `json:"interrupt_id"`
	Payload     interface{}         `json:"payload"`
	Reason      *string             `json:"reason,omitempty"`
	Path        *string             `json:"path,omitempty"`
	Data        interface{}         `json:"data"`
	Summary     *string             `json:"summary"`
}

// Media a question can refer to. Its key is a placeholder that stands for
// it in the state, the instructions or the criteria.
//
// `uri` is a `blob://` URI from a message, or a `data:` URI.
type Asset struct {
	Type AssetType `json:"type"`
	URI  string    `json:"uri"`
}

// Why something failed. One shape on every event and on the wire.
//
// There is no `retryable` field. Whether to try again is a decision about one
// attempt, not a fact about the failure. The events that settle an attempt
// carry it instead.
type ErrorInfo struct {
	Code ErrorCode `json:"code"`
	// Small structured details, such as a status or the llm blocks that
	// exist.
	Detail interface{} `json:"detail"`
	// One sentence the engine wrote, safe to show a human. Never a raw
	// document. An unbounded body belongs in the log.
	Message string `json:"message"`
	// The one input to fix, when the failure names one. For example
	// `agent.llm` or `actions[0].type`.
	Param *string `json:"param"`
}

// A named question about a `system1.call`'s state. Its type sets the type
// of its answer.
//
// Is it true? The answer is the probability of yes.
//
// Which option? `criteria` names each option and says when it applies.
//
// Where on a scale? `criteria` lists the levels in order. The first is 0.
//
// Where in the image?
type Question struct {
	Criteria     *Criteria    `json:"criteria"`
	Instructions interface{}  `json:"instructions"`
	Type         QuestionType `json:"type"`
}

// What counts as yes and what counts as no.
type NoulCriteria struct {
	False interface{} `json:"false"`
	True  interface{} `json:"true"`
}

type ReasoningConfig struct {
	Effort    *ReasoningEffort `json:"effort"`
	Enabled   *bool            `json:"enabled"`
	Exclude   *bool            `json:"exclude"`
	MaxTokens *int64           `json:"max_tokens"`
}

// A tool's declared contract. Flat on the wire. A provider that needs it
// nested re-wraps it at its own boundary.
type LlmTool struct {
	// Keep this definition out of the request.
	//
	// The engine still records it, still routes a call to it, and still finds
	// it in a search. Only the request leaves it out. That keeps a large tool
	// set out of the model's context and out of the cached prefix.
	//
	// Any source can set it. Deferral belongs to a tool, not to where the tool
	// came from.
	Defer       *bool  `json:"defer,omitempty"`
	Description string `json:"description"`
	// JSON Schema for the arguments. Absent declares a tool with no
	// arguments. The engine checks every call against it.
	Input interface{} `json:"input"`
	Name  string      `json:"name"`
	// JSON Schema the result must satisfy. The model never sees it. A result
	// that breaks it becomes a terminal tool error.
	Output interface{} `json:"output"`
}

// The trigger a worker sees on the wire. There is no `ClientMessage`: the
// engine turns a bare client message into `ClientTranscript` before it sends
// it.
//
// The first decision of every session; carries no proposal.
//
// A model call waits for room. `proposed` holds the plan of the strategy
// that applied. A model call made before `call` runs again writes the
// summary. `call` with its own id runs on whatever history is set by then.
//
// A `run_code` call's script paused on these calls. `id` is the
// `run_code` call. Answer each by its `id`: `tool.call` runs it,
// `tool.error` refuses it.
//
// Answer with `tool.result`/`tool.error`.
//
// Answer with `llm.result`/`llm.error`.
//
// The body of an interrupt resume: which interrupt, and the payload delivered
// to the worker. Shared by the [`ClientInput::InterruptResume`] input and the
// [`DecisionTrigger::InterruptResumed`] trigger.
//
// Sent after a turn completes, with its final output. The session stays
// busy until it is answered. Echo the proposed `done` to finish.
type DecisionTrigger struct {
	Type DecisionTriggerType `json:"type"`
	// Inputs the client declared on its run. The engine adds
	// `client.tools` to the proposed config.
	Client *ClientContext `json:"client,omitempty"`
	// The new messages were sent with `queue: "join"`, and join the
	// running turn.
	Join     *bool                `json:"join,omitempty"`
	Messages []DraftMessage       `json:"messages,omitempty"`
	NewFrom  *int64               `json:"new_from,omitempty"`
	Args     interface{}          `json:"args"`
	Name     *string              `json:"name,omitempty"`
	Call     *HeldCall            `json:"call,omitempty"`
	Reason   *ContextReason       `json:"reason,omitempty"`
	Strategy *ContextStrategyKind `json:"strategy"`
	// What the model would read, in tokens.
	Tokens    *int64            `json:"tokens,omitempty"`
	Window    *int64            `json:"window"`
	ID        *string           `json:"id,omitempty"`
	ToolCalls []ToolCallElement `json:"tool_calls,omitempty"`
	Arguments *string           `json:"arguments,omitempty"`
	Attempt   *int64            `json:"attempt,omitempty"`
	Deadline  *time.Time        `json:"deadline"`
	// What the engine made of `arguments` against the tool's `input`
	// schema: `valid`, `invalid`, or `malformed`. Always on the wire.
	Input  *ToolInput `json:"input,omitempty"`
	Error  *ErrorInfo `json:"error"`
	Ok     *bool      `json:"ok,omitempty"`
	Result *Result    `json:"result"`
	Format *LlmFormat `json:"format"`
	// The neutral `LlmRequest` JSON, or the provider's native request body
	// when `format` is set.
	Request interface{}   `json:"request"`
	Stream  *bool         `json:"stream,omitempty"`
	Cost    *string       `json:"cost"`
	Message *DraftMessage `json:"message"`
	// True when the model declined the request. Without it, a refusal
	// looks like a turn that ended well and said nothing.
	Refused   *bool   `json:"refused,omitempty"`
	Truncated *bool   `json:"truncated,omitempty"`
	Usage     *Usage  `json:"usage"`
	AgentID   *string `json:"agent_id,omitempty"`
	SessionID *string `json:"session_id,omitempty"`
	// One answer for each question, by the question's name.
	Answers     map[string]Answer `json:"answers,omitempty"`
	Model       *string           `json:"model"`
	InterruptID *string           `json:"interrupt_id,omitempty"`
	Payload     interface{}       `json:"payload"`
	Data        interface{}       `json:"data"`
	TurnID      *string           `json:"turn_id,omitempty"`
}

// The answer to one question. A field the engine does not name stays on
// the answer as the provider sent it.
//
// `noul` is the probability of yes, from 0 to 1.
//
// `score` is the expected level. It can fall between two levels.
//
// The edges are fractions of the image's height and width.
type Answer struct {
	Noul          *float64               `json:"noul,omitempty"`
	Type          QuestionType           `json:"type"`
	Choice        *string                `json:"choice,omitempty"`
	Confidence    *float64               `json:"confidence"`
	Probabilities map[string]float64     `json:"probabilities,omitempty"`
	Legend        map[string]interface{} `json:"legend,omitempty"`
	Score         *float64               `json:"score,omitempty"`
	Box           *Edges                 `json:"box,omitempty"`
}

type Edges struct {
	Bottom float64 `json:"bottom"`
	Left   float64 `json:"left"`
	Right  float64 `json:"right"`
	Top    float64 `json:"top"`
}

// A model call that waits for room.
type HeldCall struct {
	ID      string     `json:"id"`
	Llm     string     `json:"llm"`
	Request LlmRequest `json:"request"`
	Stream  bool       `json:"stream"`
}

type LlmRequest struct {
	MaxCompletionTokens *int64           `json:"max_completion_tokens"`
	Messages            []DraftMessage   `json:"messages"`
	Model               string           `json:"model"`
	Reasoning           *ReasoningConfig `json:"reasoning"`
	Temperature         *float64         `json:"temperature"`
	Tools               []LlmTool        `json:"tools"`
}

// What the engine made of `arguments` against the tool's `input`
// schema: `valid`, `invalid`, or `malformed`. Always on the wire.
//
// What the engine made of a tool call's arguments, sent with the raw
// `arguments` string. Always on the wire.
//
// Parsed, and valid against the `input` schema if the tool declares one.
// `value` is the parsed `arguments`. The engine never changes it.
//
// Parsed to an object that violates the declared `input` schema.
//
// Not a JSON object. Either malformed JSON or another type.
type ToolInput struct {
	Status Status      `json:"status"`
	Value  interface{} `json:"value"`
	Error  *string     `json:"error,omitempty"`
}

type StoredResult struct {
	Content           []StoredContent `json:"content,omitempty"`
	IsError           *bool           `json:"isError,omitempty"`
	StructuredContent interface{}     `json:"structuredContent"`
}

type StoredContent struct {
	Text     *string            `json:"text,omitempty"`
	Type     *StoredContentType `json:"type,omitempty"`
	URI      *string            `json:"uri,omitempty"`
	MIMEType *string            `json:"mimeType"`
	Name     *string            `json:"name"`
}

// What one call read and wrote. Every provider means these counts the same
// way.
//
// Vendors report different things. Anthropic gives the part of the prompt it
// did not read from the cache. OpenAI gives the whole prompt. Each adapter
// converts to this shape, because these counts get added together.
type Usage struct {
	// The part of `input` the provider read from the cache.
	CacheRead int64 `json:"cache_read"`
	// The part of `input` the provider wrote to the cache.
	CacheWrite int64 `json:"cache_write"`
	// Every input token of the call, cached or not.
	Input  int64 `json:"input"`
	Output int64 `json:"output"`
	// The counts as the provider reported them, for a number this type does
	// not name.
	Provider interface{} `json:"provider"`
	// `input` and `output` together.
	Total int64 `json:"total"`
	// The part of `input` the provider read fresh.
	UncachedInput int64 `json:"uncached_input"`
}

type TurnClass struct {
	ID string `json:"id"`
	// The `metadata` the turn was submitted with.
	Metadata map[string]string `json:"metadata,omitempty"`
}

// Which worker identity decides for a session, and optionally at what
// address. `url` fills in when the declared block has none, or overrides it.
type WorkerRef struct {
	ID  string  `json:"id"`
	URL *string `json:"url"`
}

// An interrupt payload. `id` and `reason` live on the interrupt itself.
type InterruptPayload struct {
	// RFC 3339. Display only.
	ExpiresAt *string `json:"expiresAt"`
	// Markdown. A channel converts it as it needs. Without it, a channel
	// shows the interrupt's `reason`.
	Message *string `json:"message"`
	// Free-form, delivered to clients unchanged. `metadata.options` is a
	// list of [`InterruptOption`], which a channel can show as buttons.
	Metadata interface{} `json:"metadata"`
	// JSON Schema for the expected resolution payload.
	ResponseSchema interface{} `json:"responseSchema"`
	// Binds the interrupt to a prior tool call.
	ToolCallID *string `json:"toolCallId"`
}

// A resume payload a channel wrote, with who resolved it.
type InterruptResolution struct {
	Payload   interface{}         `json:"payload"`
	Responder *InterruptResponder `json:"responder"`
	Status    ResumeStatus        `json:"status"`
}

// Who resolved it. The channel sets this, never the requester.
type InterruptResponder struct {
	// The channel that resolved it.
	Channel string `json:"channel"`
	// The chosen option's label, when the resolution was a pick.
	Label *string `json:"label"`
	// The chosen option's `style`, when the resolution was a pick.
	Style *string `json:"style"`
	// Channel-native user id.
	User *string `json:"user"`
}

type StreamDelta struct {
	FinishReason *string         `json:"finish_reason"`
	Reasoning    *string         `json:"reasoning"`
	Text         *string         `json:"text"`
	ToolCalls    []ToolCallChunk `json:"tool_calls,omitempty"`
}

type ToolCallChunk struct {
	Arguments *string `json:"arguments"`
	ID        string  `json:"id"`
	Name      *string `json:"name"`
}

type TokenDelta struct {
	AgentID      string  `json:"agent_id"`
	Attempt      int64   `json:"attempt"`
	CallID       string  `json:"call_id"`
	FinishReason *string `json:"finish_reason"`
	Reasoning    *string `json:"reasoning"`
	// Transport routing key.
	RootSessionID string `json:"root_session_id"`
	// Per-call counter, distinct from event-store sequence.
	Seq int64 `json:"seq"`
	// May be a subagent of root.
	SessionID string `json:"session_id"`
	// Tenant isolation key — subscribers must match.
	TenantID  string          `json:"tenant_id"`
	Text      *string         `json:"text"`
	ToolCalls []ToolCallChunk `json:"tool_calls"`
	TurnID    *string         `json:"turn_id"`
}

// The engine makes the call.
//
// The worker executes it.
//
// The client executes it. The session goes idle until it answers.
// Tools only.
type Handler string

const (
	Client        Handler = "client"
	HandlerWorker Handler = "worker"
	Server        Handler = "server"
)

type ToolContentType string

const (
	Audio        ToolContentType = "audio"
	PurpleImage  ToolContentType = "image"
	PurpleText   ToolContentType = "text"
	Resource     ToolContentType = "resource"
	ResourceLink ToolContentType = "resource_link"
)

type ContentPartWireType string

const (
	File       ContentPartWireType = "file"
	FluffyText ContentPartWireType = "text"
	ImageURL   ContentPartWireType = "image_url"
	InputAudio ContentPartWireType = "input_audio"
	PurpleBlob ContentPartWireType = "blob"
	PurpleLink ContentPartWireType = "link"
	VideoURL   ContentPartWireType = "video_url"
)

// Which provider wrote the blocks. They go back only to that provider.
// Anthropic rejects blocks it did not sign.
type ReasoningProvider string

const (
	Openrouter                 ReasoningProvider = "openrouter"
	ReasoningProviderAnthropic ReasoningProvider = "anthropic"
	ReasoningProviderOpenai    ReasoningProvider = "openai"
)

type Role string

const (
	Assistant Role = "assistant"
	System    Role = "system"
	Tool      Role = "tool"
	User      Role = "user"
)

type QueueWord string

const (
	Join  QueueWord = "join"
	Merge QueueWord = "merge"
)

type ClientInputType string

const (
	InterruptResume      ClientInputType = "interrupt.resume"
	PurpleClientAction   ClientInputType = "client.action"
	PurpleClientAppend   ClientInputType = "client.append"
	PurpleClientMessage  ClientInputType = "client.message"
	PurpleClientMessages ClientInputType = "client.messages"
	PurpleToolError      ClientInputType = "tool.error"
	PurpleToolResult     ClientInputType = "tool.result"
)

type ClientPayloadType string

const (
	FluffyClientAction   ClientPayloadType = "client.action"
	FluffyClientAppend   ClientPayloadType = "client.append"
	FluffyClientMessage  ClientPayloadType = "client.message"
	FluffyClientMessages ClientPayloadType = "client.messages"
)

type Disposition string

const (
	Attachment Disposition = "attachment"
	Inline     Disposition = "inline"
)

type AttachmentTool string

const (
	Read AttachmentTool = "read"
	View AttachmentTool = "view"
)

// The language a script is written in. Each has a dialect that describes
// `run_code` to the model and an engine that runs it.
type CodemodeEnum string

const (
	Javascript CodemodeEnum = "javascript"
)

// Start over with only the turn's own messages.
//
// Summarize the older messages and keep the recent ones.
//
// Leave the plan to the worker.
type ContextStrategyKind string

const (
	ContextStrategyKindWorker ContextStrategyKind = "worker"
	Reset                     ContextStrategyKind = "reset"
	Summarize                 ContextStrategyKind = "summarize"
)

// How the model learns the agent's skills.
//
// `list` names every skill in the system prompt. `search` names none there:
// `tool_search` finds skills beside tools, so a growing set of skills costs
// the prompt nothing.
type SkillListing string

const (
	List               SkillListing = "list"
	SkillListingSearch SkillListing = "search"
)

// Which tools the agent gets to reach the ones it defers.
//
// How the tools an agent defers reach the model.
//
// The engine holds every deferred definition whatever this says. This chooses
// which tools the request advertises, and whether the request carries the
// deferred definitions.
//
// `tool_search` and `call_tool`. A search answers with the schema, so one
// search is enough to make a call.
type DeferToolsStrategy string

const (
	DeferToolsStrategySearch DeferToolsStrategy = "search"
)

type ReasoningEffort string

const (
	High    ReasoningEffort = "high"
	Low     ReasoningEffort = "low"
	Medium  ReasoningEffort = "medium"
	Minimal ReasoningEffort = "minimal"
	None    ReasoningEffort = "none"
	Xhigh   ReasoningEffort = "xhigh"
)

// Which of a connection's calls stop for a person.
//
// A tool that the connection marks `destructiveHint`.
type Approve string

const (
	Always       Approve = "always"
	ApproveNever Approve = "never"
	Destructive  Approve = "destructive"
)

// What a session does when a connection needs a person to authorize it.
//
// Stop and ask. A channel that cannot show the question degrades.
//
// Go on without this connection's tools.
type MCPAuthFailure string

const (
	Degrade                 MCPAuthFailure = "degrade"
	MCPAuthFailureInterrupt MCPAuthFailure = "interrupt"
)

// Whether the model is told that a connection's tool fetch failed. The turn
// goes ahead without those tools either way.
//
// Name the connection wherever its tools would have been.
//
// Say nothing. For a connection the agent does not need.
type MCPToolSyncFailure string

const (
	Silent MCPToolSyncFailure = "silent"
	Warn   MCPToolSyncFailure = "warn"
)

// Whether the engine tells the model that an MCP server is available, and
// what that server says it is for.
//
// Where an MCP announcement lands.
//
// The system prompt while no call has run. Then a block on the last user
// message. Then a message of its own. The engine takes the first place it
// can use.
//
// Nowhere. For a server whose own description does not help.
type MCPAnnounce string

const (
	Auto             MCPAnnounce = "auto"
	MCPAnnounceNever MCPAnnounce = "never"
)

// Where its skills come from. Absent ⇒ its bundle.
//
// Where a skill comes from: the bundle `subs apply` sent, or the worker,
// which lists it in the config it returns and answers each `skill` call as a
// `tool.execute`, so a skill can say what is true now.
//
// Absent ⇒ the project's skills bundle, which holds every `[skill.<id>]`
// with a `path`.
type SkillSource string

const (
	Bundle            SkillSource = "bundle"
	SkillSourceWorker SkillSource = "worker"
)

// How the model reaches an agent's subagents.
//
// One tool per subagent, named by the agent.
//
// One `subagent` tool for all of them. The call names the agent.
type SubagentToolsStrategy string

const (
	PerAgent SubagentToolsStrategy = "per_agent"
	Single   SubagentToolsStrategy = "single"
)

// How calls to a subagent return. Configuration; each call carries a
// [`SpawnMode`].
type SubagentMode string

const (
	Any                  SubagentMode = "any"
	SubagentModeBlocking SubagentMode = "blocking"
	SubagentModeDetached SubagentMode = "detached"
)

// What kind of work an effect is. One enum for the wire and for scheduling. A
// decision and a turn's end queue beside the calls and are swept the same way,
// so they are kinds too. Neither appears on an [`Effect`].
//
// Fetching one connection's tool list. Its `id` is the connection id.
//
// A worker decision.
//
// The turn's completion, which waits for the `turn.finished` decision to
// settle. Carries the turn id. Never swept, because it has no deadline.
//
// A call to a `[system1.*]` block.
type EffectKind string

const (
	ConnectorSync EffectKind = "connector_sync"
	Decision      EffectKind = "decision"
	LlmCall       EffectKind = "llm_call"
	Subagent      EffectKind = "subagent"
	System1Call   EffectKind = "system1_call"
	ToolCall      EffectKind = "tool_call"
	TurnEnd       EffectKind = "turn_end"
)

// Running, waiting for its result. Off the deadline clock. A subagent
// stays here for as long as its child turn takes.
type EffectStatus string

const (
	Completed      EffectStatus = "completed"
	Failed         EffectStatus = "failed"
	Pending        EffectStatus = "pending"
	Queued         EffectStatus = "queued"
	RetryScheduled EffectStatus = "retry_scheduled"
	Running        EffectStatus = "running"
)

// Who can read what a session says. The transport sets it once, when the
// session starts. Absent or unknown reads as `shared`. `shared` never selects
// a personal credential.
//
// More than one person can read the answer.
//
// One person only.
type Visibility string

const (
	Private Visibility = "private"
	Shared  Visibility = "shared"
)

type AssetType string

const (
	FluffyImage AssetType = "image"
)

// What kind of failure, so a consumer can branch on it instead of reading the
// sentence. A closed set, required on every [`ErrorInfo`].
//
// `provider_error`, `rate_limited`, `refused`, `budget_exceeded`, and
// `deadline_exceeded` mean a call ran and went wrong.
// `invalid_response` means a document did not parse, or parsed into something
// unusable. `handler_error` means the worker or client reported its own
// failure. `worker_unreachable` means it was never reached. `unroutable`
// means nothing could decide. `internal` means the engine's own fault.
// `context_overflow` means the request was longer than the model can read.
type ErrorCode string

const (
	BudgetExceeded     ErrorCode = "budget_exceeded"
	ContextOverflow    ErrorCode = "context_overflow"
	DeadlineExceeded   ErrorCode = "deadline_exceeded"
	ErrorCodeCancelled ErrorCode = "cancelled"
	HandlerError       ErrorCode = "handler_error"
	Internal           ErrorCode = "internal"
	InvalidResponse    ErrorCode = "invalid_response"
	ProviderError      ErrorCode = "provider_error"
	RateLimited        ErrorCode = "rate_limited"
	Refused            ErrorCode = "refused"
	Unroutable         ErrorCode = "unroutable"
	WorkerUnreachable  ErrorCode = "worker_unreachable"
)

// How one subagent call returns.
type SpawnMode string

const (
	SpawnModeBlocking SpawnMode = "blocking"
	SpawnModeDetached SpawnMode = "detached"
	Wait              SpawnMode = "wait"
)

type QuestionType string

const (
	Box    QuestionType = "box"
	Choice QuestionType = "choice"
	Noul   QuestionType = "noul"
	Score  QuestionType = "score"
)

type DecisionActionType string

const (
	ContextReplace    DecisionActionType = "context.replace"
	Done              DecisionActionType = "done"
	Fail              DecisionActionType = "fail"
	FluffyToolError   DecisionActionType = "tool.error"
	FluffyToolResult  DecisionActionType = "tool.result"
	InterruptResolve  DecisionActionType = "interrupt.resolve"
	LlmError          DecisionActionType = "llm.error"
	LlmResult         DecisionActionType = "llm.result"
	MessageSend       DecisionActionType = "message.send"
	SubagentSpawn     DecisionActionType = "subagent.spawn"
	TypeConnectorSync DecisionActionType = "connector.sync"
	TypeInterrupt     DecisionActionType = "interrupt"
	TypeLlmCall       DecisionActionType = "llm.call"
	TypeSystem1Call   DecisionActionType = "system1.call"
	TypeToolCall      DecisionActionType = "tool.call"
)

// OpenAI Chat Completions.
//
// Anthropic Messages API.
type LlmFormat string

const (
	LlmFormatAnthropic LlmFormat = "anthropic"
	LlmFormatOpenai    LlmFormat = "openai"
)

type Status string

const (
	Invalid   Status = "invalid"
	Malformed Status = "malformed"
	Valid     Status = "valid"
)

// Why the engine stopped before a model call to make room.
//
// The session was quiet for longer than `idle_secs`.
//
// The model would read more than the strategy allows.
//
// The provider refused the call as too long.
type ContextReason string

const (
	Full     ContextReason = "full"
	Idle     ContextReason = "idle"
	Overflow ContextReason = "overflow"
)

type StoredContentType string

const (
	FluffyBlob    StoredContentType = "blob"
	FluffyLink    StoredContentType = "link"
	TentacledText StoredContentType = "text"
)

type DecisionTriggerType string

const (
	CodemodeCalls           DecisionTriggerType = "codemode.calls"
	ContextLimit            DecisionTriggerType = "context.limit"
	InterruptResumed        DecisionTriggerType = "interrupt.resumed"
	LlmExecute              DecisionTriggerType = "llm.execute"
	LlmFinished             DecisionTriggerType = "llm.finished"
	SessionStart            DecisionTriggerType = "session.start"
	SubagentFinished        DecisionTriggerType = "subagent.finished"
	System1Finished         DecisionTriggerType = "system1.finished"
	TentacledClientAction   DecisionTriggerType = "client.action"
	TentacledClientMessages DecisionTriggerType = "client.messages"
	ToolExecute             DecisionTriggerType = "tool.execute"
	ToolFinished            DecisionTriggerType = "tool.finished"
	TurnFinished            DecisionTriggerType = "turn.finished"
)

type ResumeStatus string

const (
	Resolved              ResumeStatus = "resolved"
	ResumeStatusCancelled ResumeStatus = "cancelled"
)

type Content struct {
	ContentPartWireArray []ContentPartWire
	String               *string
}

func (x *Content) UnmarshalJSON(data []byte) error {
	x.ContentPartWireArray = nil
	object, err := unmarshalUnion(data, nil, nil, nil, &x.String, true, &x.ContentPartWireArray, false, nil, false, nil, false, nil, true)
	if err != nil {
		return err
	}
	if object {
	}
	return nil
}

func (x *Content) MarshalJSON() ([]byte, error) {
	return marshalUnion(nil, nil, nil, x.String, x.ContentPartWireArray != nil, x.ContentPartWireArray, false, nil, false, nil, false, nil, true)
}

// What a message does while a turn runs. `false` or `null` refuses it. `true` holds it for
// a turn of its own. `"merge"` adds it to the turn waiting last when that turn also merges,
// so messages sent during a turn become one turn. `"join"` adds it to the running turn,
// which sees it at its next model call.
type Queue struct {
	Bool *bool
	Enum *QueueWord
}

func (x *Queue) UnmarshalJSON(data []byte) error {
	x.Enum = nil
	object, err := unmarshalUnion(data, nil, nil, &x.Bool, nil, false, nil, false, nil, false, nil, true, &x.Enum, true)
	if err != nil {
		return err
	}
	if object {
	}
	return nil
}

func (x *Queue) MarshalJSON() ([]byte, error) {
	return marshalUnion(nil, nil, x.Bool, nil, false, nil, false, nil, false, nil, x.Enum != nil, x.Enum, true)
}

type Size struct {
	Integer *int64
	String  *string
}

func (x *Size) UnmarshalJSON(data []byte) error {
	object, err := unmarshalUnion(data, &x.Integer, nil, nil, &x.String, false, nil, false, nil, false, nil, false, nil, true)
	if err != nil {
		return err
	}
	if object {
	}
	return nil
}

func (x *Size) MarshalJSON() ([]byte, error) {
	return marshalUnion(x.Integer, nil, nil, x.String, false, nil, false, nil, false, nil, false, nil, true)
}

// Defer every tool this agent offers, whatever its source. A tool or a
// connection overrides this with its own `defer`. Absent, the agent defers
// nothing; a connection can still defer on its own.
type DeferToolsWire struct {
	Bool       *bool
	DeferTools *DeferTools
}

func (x *DeferToolsWire) UnmarshalJSON(data []byte) error {
	x.DeferTools = nil
	var c DeferTools
	object, err := unmarshalUnion(data, nil, nil, &x.Bool, nil, false, nil, true, &c, false, nil, false, nil, true)
	if err != nil {
		return err
	}
	if object {
		x.DeferTools = &c
	}
	return nil
}

func (x *DeferToolsWire) MarshalJSON() ([]byte, error) {
	return marshalUnion(nil, nil, x.Bool, nil, false, nil, x.DeferTools != nil, x.DeferTools, false, nil, false, nil, true)
}

type Error struct {
	ErrorInfo *ErrorInfo
	String    *string
}

func (x *Error) UnmarshalJSON(data []byte) error {
	x.ErrorInfo = nil
	var c ErrorInfo
	object, err := unmarshalUnion(data, nil, nil, nil, &x.String, false, nil, true, &c, false, nil, false, nil, false)
	if err != nil {
		return err
	}
	if object {
		x.ErrorInfo = &c
	}
	return nil
}

func (x *Error) MarshalJSON() ([]byte, error) {
	return marshalUnion(nil, nil, nil, x.String, false, nil, x.ErrorInfo != nil, x.ErrorInfo, false, nil, false, nil, false)
}

type Criteria struct {
	AnythingArray []interface{}
	NoulCriteria  *NoulCriteria
}

func (x *Criteria) UnmarshalJSON(data []byte) error {
	x.AnythingArray = nil
	x.NoulCriteria = nil
	var c NoulCriteria
	object, err := unmarshalUnion(data, nil, nil, nil, nil, true, &x.AnythingArray, true, &c, false, nil, false, nil, true)
	if err != nil {
		return err
	}
	if object {
		x.NoulCriteria = &c
	}
	return nil
}

func (x *Criteria) MarshalJSON() ([]byte, error) {
	return marshalUnion(nil, nil, nil, nil, x.AnythingArray != nil, x.AnythingArray, x.NoulCriteria != nil, x.NoulCriteria, false, nil, false, nil, true)
}

type Result struct {
	StoredResult *StoredResult
	String       *string
}

func (x *Result) UnmarshalJSON(data []byte) error {
	x.StoredResult = nil
	var c StoredResult
	object, err := unmarshalUnion(data, nil, nil, nil, &x.String, false, nil, true, &c, false, nil, false, nil, true)
	if err != nil {
		return err
	}
	if object {
		x.StoredResult = &c
	}
	return nil
}

func (x *Result) MarshalJSON() ([]byte, error) {
	return marshalUnion(nil, nil, nil, x.String, false, nil, x.StoredResult != nil, x.StoredResult, false, nil, false, nil, true)
}

func unmarshalUnion(data []byte, pi **int64, pf **float64, pb **bool, ps **string, haveArray bool, pa interface{}, haveObject bool, pc interface{}, haveMap bool, pm interface{}, haveEnum bool, pe interface{}, nullable bool) (bool, error) {
	if pi != nil {
		*pi = nil
	}
	if pf != nil {
		*pf = nil
	}
	if pb != nil {
		*pb = nil
	}
	if ps != nil {
		*ps = nil
	}

	dec := json.NewDecoder(bytes.NewReader(data))
	dec.UseNumber()
	tok, err := dec.Token()
	if err != nil {
		return false, err
	}

	switch v := tok.(type) {
	case json.Number:
		if pi != nil {
			i, err := v.Int64()
			if err == nil {
				*pi = &i
				return false, nil
			}
		}
		if pf != nil {
			f, err := v.Float64()
			if err == nil {
				*pf = &f
				return false, nil
			}
			return false, errors.New("Unparsable number")
		}
		return false, errors.New("Union does not contain number")
	case float64:
		return false, errors.New("Decoder should not return float64")
	case bool:
		if pb != nil {
			*pb = &v
			return false, nil
		}
		return false, errors.New("Union does not contain bool")
	case string:
		if haveEnum {
			return false, json.Unmarshal(data, pe)
		}
		if ps != nil {
			*ps = &v
			return false, nil
		}
		return false, errors.New("Union does not contain string")
	case nil:
		if nullable {
			return false, nil
		}
		return false, errors.New("Union does not contain null")
	case json.Delim:
		if v == '{' {
			if haveObject {
				return true, json.Unmarshal(data, pc)
			}
			if haveMap {
				return false, json.Unmarshal(data, pm)
			}
			return false, errors.New("Union does not contain object")
		}
		if v == '[' {
			if haveArray {
				return false, json.Unmarshal(data, pa)
			}
			return false, errors.New("Union does not contain array")
		}
		return false, errors.New("Cannot handle delimiter")
	}
	return false, errors.New("Cannot unmarshal union")
}

func marshalUnion(pi *int64, pf *float64, pb *bool, ps *string, haveArray bool, pa interface{}, haveObject bool, pc interface{}, haveMap bool, pm interface{}, haveEnum bool, pe interface{}, nullable bool) ([]byte, error) {
	if pi != nil {
		return json.Marshal(*pi)
	}
	if pf != nil {
		return json.Marshal(*pf)
	}
	if pb != nil {
		return json.Marshal(*pb)
	}
	if ps != nil {
		return json.Marshal(*ps)
	}
	if haveArray {
		return json.Marshal(pa)
	}
	if haveObject {
		return json.Marshal(pc)
	}
	if haveMap {
		return json.Marshal(pm)
	}
	if haveEnum {
		return json.Marshal(pe)
	}
	if nullable {
		return json.Marshal(nil)
	}
	return nil, errors.New("Union must not be null")
}
