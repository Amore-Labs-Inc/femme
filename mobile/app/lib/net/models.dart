/// The replicated state of femme-proto (`state.rs`) as Dart classes. Field names follow the Rust
/// structs; `fromJson`/`toJson` speak serde_json's encoding of them.
library;

import 'dart:convert';

import 'wire.dart';

export 'rows.dart';

enum Agent { shell, claude, codex, pi }

enum Mode { headless, tui }

enum SessionStatus {
  starting,
  idle,
  working,
  blocked,
  paused,
  interrupted,
  crashed,
  failed,
  closed,
}

enum Attention { none, blocked, done }

enum GoalState { active, notMet, met, cleared }

enum PaneKind { agent, shell }

enum ItemKind {
  userMessage,
  peerMessage,
  assistantMessage,
  thinking,
  toolCall,
  toolResult,
  question,
  plan,
  event,
  turnStart,
  turnEnd,
}

enum ItemStatus { inProgress, completed, failed, pending, answered }

enum IssueStatus { open, working, waiting, paused, pr, done, failed }

enum Entity { session, pane, item, issue }

class PaneId {
  const PaneId(this.sid, this.no);
  final String sid;
  final int no;

  factory PaneId.fromJson(Json j) => PaneId(j['sid'] as String, j['no'] as int);
  Json toJson() => {'sid': sid, 'no': no};

  /// The id a pane tombstone carries (`sid:no`).
  String get key => '$sid:$no';

  @override
  bool operator ==(Object other) =>
      other is PaneId && other.sid == sid && other.no == no;
  @override
  int get hashCode => Object.hash(sid, no);
  @override
  String toString() => key;
}

class Context {
  const Context({
    required this.used,
    required this.window,
    this.compactions = 0,
    this.compactAt,
  });
  final int used;
  final int window;
  final int compactions;

  /// Tokens at which the agent compacts by itself (Claude's `autoCompactWindow` minus its
  /// buffer); null until it said.
  final int? compactAt;

  factory Context.fromJson(Json j) => Context(
    used: j['used'] as int,
    window: j['window'] as int,
    compactions: j['compactions'] as int? ?? 0,
    compactAt: j['compact_at'] as int?,
  );
  Json toJson() => {
    'used': used,
    'window': window,
    'compactions': compactions,
    if (compactAt != null) 'compact_at': compactAt,
  };

  /// What fills the bar: where the agent compacts by itself when it said, else the window.
  int get full => (compactAt ?? 0) > 0 ? compactAt! : window;

  /// 0..1 on the way to the next compaction, or null before the agent reported a size.
  double? get fraction => full == 0 ? null : (used / full).clamp(0.0, 1.0);
}

class Goal {
  const Goal({required this.condition, required this.state, this.reason});
  final String condition;
  final GoalState state;
  final String? reason;

  factory Goal.fromJson(Json j) => Goal(
    condition: j['condition'] as String,
    state: wireEnum(GoalState.values, j['state'], GoalState.active),
    reason: j['reason'] as String?,
  );
  Json toJson() => {
    'condition': condition,
    'state': wireName(state),
    'reason': reason,
  };
}

class Session {
  const Session({
    required this.id,
    required this.handle,
    required this.rev,
    this.name,
    required this.owner,
    required this.agent,
    required this.mode,
    required this.cwd,
    this.label,
    this.agentSid,
    this.model,
    required this.status,
    required this.attention,
    required this.created,
    this.closed,
    this.permission,
    this.context,
    this.effort,
    this.goal,
  });

  final String id;
  final String handle;
  final int rev;
  final String? name;
  final String owner;
  final Agent agent;
  final Mode mode;
  final String cwd;
  final String? label;
  final String? agentSid;
  final String? model;
  final SessionStatus status;
  final Attention attention;
  final int created;
  final int? closed;
  final String? permission;
  final Context? context;
  final String? effort;
  final Goal? goal;

  /// What a session is called, as the desktop (`desktop/src/net.rs` `title`): its name, else its
  /// label, else the last component of its folder, else its handle.
  String get title {
    if (name != null && name!.isNotEmpty) return name!;
    if (label != null && label!.isNotEmpty) return label!;
    final trimmed = cwd.replaceFirst(RegExp(r'/+$'), '');
    final tail = trimmed.substring(trimmed.lastIndexOf('/') + 1);
    return tail.isNotEmpty ? tail : handle;
  }

  bool get needsYou =>
      status == SessionStatus.blocked || attention == Attention.blocked;

  factory Session.fromJson(Json j) => Session(
    id: j['id'] as String,
    handle: j['handle'] as String,
    rev: j['rev'] as int,
    name: j['name'] as String?,
    owner: j['owner'] as String,
    agent: wireEnum(Agent.values, j['agent'], Agent.shell),
    mode: wireEnum(Mode.values, j['mode'], Mode.headless),
    cwd: j['cwd'] as String,
    label: j['label'] as String?,
    agentSid: j['agent_sid'] as String?,
    model: j['model'] as String?,
    status: wireEnum(SessionStatus.values, j['status'], SessionStatus.idle),
    attention: wireEnum(Attention.values, j['attention'], Attention.none),
    created: j['created'] as int,
    closed: j['closed'] as int?,
    permission: j['permission'] as String?,
    context: j['context'] == null
        ? null
        : Context.fromJson(j['context'] as Json),
    effort: j['effort'] as String?,
    goal: j['goal'] == null ? null : Goal.fromJson(j['goal'] as Json),
  );

  Json toJson() => {
    'id': id,
    'handle': handle,
    'rev': rev,
    'name': name,
    'owner': owner,
    'agent': wireName(agent),
    'mode': wireName(mode),
    'cwd': cwd,
    'label': label,
    'agent_sid': agentSid,
    'model': model,
    'status': wireName(status),
    'attention': wireName(attention),
    'created': created,
    'closed': closed,
    'permission': permission,
    'context': context?.toJson(),
    'effort': effort,
    'goal': goal?.toJson(),
  };
}

class Pane {
  const Pane({
    required this.id,
    required this.rev,
    required this.kind,
    required this.title,
  });
  final PaneId id;
  final int rev;
  final PaneKind kind;
  final String title;

  factory Pane.fromJson(Json j) => Pane(
    id: PaneId.fromJson(j['id'] as Json),
    rev: j['rev'] as int,
    kind: wireEnum(PaneKind.values, j['kind'], PaneKind.shell),
    title: j['title'] as String,
  );
  Json toJson() => {
    'id': id.toJson(),
    'rev': rev,
    'kind': wireName(kind),
    'title': title,
  };
}

/// `rpc::Answer`.
class Answer {
  const Answer._(this.json);
  final Object json;

  factory Answer.option(int n) => Answer._({'Option': n});
  factory Answer.options(List<int> ns) => Answer._({'Options': ns});
  factory Answer.text(String t) => Answer._({'Text': t});
  static const cancel = Answer._('Cancel');

  factory Answer.fromJson(Object j) => Answer._(j);

  /// The picked option for `Option`, null otherwise.
  int? get option => json is Map ? (json as Map)['Option'] as int? : null;
  List<int>? get options =>
      json is Map ? ((json as Map)['Options'] as List?)?.cast<int>() : null;
  String? get text => json is Map ? (json as Map)['Text'] as String? : null;
  bool get isCancel => json == 'Cancel';
}

/// A `Question` item's body (`femme-agents` `QuestionItem`: the question flattened, plus the
/// answer once given).
class Question {
  const Question({
    required this.requestId,
    required this.prompt,
    required this.options,
    required this.multi,
    required this.freeText,
    this.answered,
    this.reason,
  });
  final String requestId;
  final String prompt;
  final List<String> options;
  final bool multi;
  final bool freeText;
  final Answer? answered;
  final String? reason;

  factory Question.fromJson(Json j) => Question(
    requestId: j['request_id'] as String,
    prompt: j['prompt'] as String? ?? '',
    options: (j['options'] as List? ?? const []).cast<String>(),
    multi: j['multi'] as bool? ?? false,
    freeText: j['free_text'] as bool? ?? false,
    answered: j['answered'] == null
        ? null
        : Answer.fromJson(j['answered'] as Object),
    reason: j['reason'] as String?,
  );
}

class Item {
  Item({
    required this.sid,
    required this.itemId,
    required this.seq,
    required this.rev,
    required this.kind,
    required this.status,
    required this.preview,
    this.body,
    this.blobHash,
    required this.ts,
    this.parent,
  });

  final String sid;
  final String itemId;
  final int seq;
  final int rev;
  final ItemKind kind;
  final ItemStatus status;
  final String preview;

  /// JSON text, shaped per kind (see `femme-agents/src/items.rs`); [bodyJson] decodes it.
  final String? body;
  final List<int>? blobHash;
  final int ts;
  final String? parent;

  Json? _decoded;
  bool _didDecode = false;

  /// [body] decoded, or null when absent or not an object.
  Json? get bodyJson {
    if (!_didDecode) {
      _didDecode = true;
      if (body != null) {
        try {
          final v = jsonDecode(body!);
          if (v is Json) _decoded = v;
        } on FormatException {
          _decoded = null;
        }
      }
    }
    return _decoded;
  }

  /// The body's `text` (user, assistant, thinking, plan, event), else the preview.
  String get text => bodyJson?['text'] as String? ?? preview;

  /// For a `UserMessage` sent by a client: the op id of the `Prompt` that sent it.
  String? get op => bodyJson?['op'] as String?;

  /// For a `UserMessage` the agent has read: when, unix ms.
  int? get readAt => bodyJson?['read_at'] as int?;

  Question? get question {
    if (kind != ItemKind.question) return null;
    final b = bodyJson;
    return b == null || b['request_id'] == null ? null : Question.fromJson(b);
  }

  factory Item.fromJson(Json j) => Item(
    sid: j['sid'] as String,
    itemId: j['item_id'] as String,
    seq: j['seq'] as int,
    rev: j['rev'] as int,
    kind: wireEnum(ItemKind.values, j['kind'], ItemKind.event),
    status: wireEnum(ItemStatus.values, j['status'], ItemStatus.completed),
    preview: j['preview'] as String? ?? '',
    body: j['body'] as String?,
    blobHash: bytesOrNull(j['blob_hash']),
    ts: j['ts'] as int,
    parent: j['parent'] as String?,
  );

  Json toJson() => {
    'sid': sid,
    'item_id': itemId,
    'seq': seq,
    'rev': rev,
    'kind': wireName(kind),
    'status': wireName(status),
    'preview': preview,
    'body': body,
    'blob_hash': blobHash,
    'ts': ts,
    'parent': parent,
  };
}

class Issue {
  const Issue({
    required this.id,
    required this.rev,
    required this.repo,
    required this.title,
    required this.body,
    required this.status,
    this.prUrl,
    this.note,
    this.session,
    this.resumeAt,
    required this.created,
    required this.updated,
  });
  final int id;
  final int rev;
  final String repo;
  final String title;
  final String body;
  final IssueStatus status;
  final String? prUrl;
  final String? note;
  final String? session;
  final int? resumeAt;
  final int created;
  final int updated;

  factory Issue.fromJson(Json j) => Issue(
    id: j['id'] as int,
    rev: j['rev'] as int,
    repo: j['repo'] as String,
    title: j['title'] as String,
    body: j['body'] as String,
    status: wireEnum(IssueStatus.values, j['status'], IssueStatus.open),
    prUrl: j['pr_url'] as String?,
    note: j['note'] as String?,
    session: j['session'] as String?,
    resumeAt: j['resume_at'] as int?,
    created: j['created'] as int,
    updated: j['updated'] as int,
  );
  Json toJson() => {
    'id': id,
    'rev': rev,
    'repo': repo,
    'title': title,
    'body': body,
    'status': wireName(status),
    'pr_url': prUrl,
    'note': note,
    'session': session,
    'resume_at': resumeAt,
    'created': created,
    'updated': updated,
  };
}
