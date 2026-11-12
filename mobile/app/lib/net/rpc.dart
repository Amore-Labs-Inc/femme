/// The control protocol of femme-proto (`rpc.rs`): requests, responses and server pushes.
library;

import 'models.dart';
import 'wire.dart';

const protoVersion = 1;

/// `rpc::Scope`: `Global` (sessions, panes, issues) or one session's transcript.
class Scope {
  const Scope._(this.sid);
  static const global = Scope._(null);
  const Scope.session(String this.sid);

  /// Null for the global scope.
  final String? sid;
  bool get isGlobal => sid == null;

  factory Scope.fromJson(Object json) {
    final (tag, v) = tagOf(json);
    return tag == 'Session' ? Scope.session(v as String) : global;
  }
  Object toJson() => sid == null ? 'Global' : {'Session': sid};

  @override
  bool operator ==(Object other) => other is Scope && other.sid == sid;
  @override
  int get hashCode => sid.hashCode;
  @override
  String toString() => sid == null ? 'Global' : 'Session($sid)';
}

class Cursors {
  const Cursors({
    required this.epoch,
    this.globalRev,
    this.sessions = const {},
  });
  final int epoch;
  final int? globalRev;
  final Map<String, int> sessions;

  Json toJson() => {
    'epoch': epoch,
    'global_rev': globalRev,
    'sessions': [
      for (final e in sessions.entries) [e.key, e.value],
    ],
  };
}

enum PromptHow { queue, steer, interruptThenSend }

class Attachment {
  const Attachment(this.name, this.blob);
  final String name;

  /// The 32-byte hash `BlobPut` answered.
  final List<int> blob;
  Json toJson() => {'name': name, 'blob': blob};
}

/// `rpc::IssueField`, one field of an `IssuePatch`.
abstract final class IssueField {
  static Object repo(String v) => {'Repo': v};
  static Object title(String v) => {'Title': v};
  static Object body(String v) => {'Body': v};
  static Object status(IssueStatus v) => {'Status': wireName(v)};
  static Object note(String? v) => {'Note': v};
  static Object prUrl(String? v) => {'PrUrl': v};
  static Object session(String? sid) => {'Session': sid};
  static Object resumeAt(int? ms) => {'ResumeAt': ms};
}

/// One builder per `rpc::Req` variant; each returns the JSON value of the request.
abstract final class Req {
  static Object subscribe(Scope scope, {int? after}) => {
    'Subscribe': {'scope': scope.toJson(), 'after': after},
  };
  static Object unsubscribe(Scope scope) => {
    'Unsubscribe': {'scope': scope.toJson()},
  };
  static Object transcriptPage(String sid, int beforeSeq, {int limit = 60}) => {
    'TranscriptPage': {'sid': sid, 'before_seq': beforeSeq, 'limit': limit},
  };
  static Object blobGet(List<int> hash) => {
    'BlobGet': {'hash': hash},
  };
  static Object sessionCreate({
    required String cwd,
    Agent agent = Agent.claude,
    Mode mode = Mode.headless,
    String? label,
    String? name,
    String? prompt,
    String? model,
  }) => {
    'SessionCreate': {
      'cwd': cwd,
      'agent': wireName(agent),
      'mode': wireName(mode),
      'label': label,
      'name': name,
      'prompt': prompt,
      'model': model,
    },
  };
  static Object sessionClose(String sid) => {
    'SessionClose': {'sid': sid},
  };
  static Object prompt(
    String sid,
    String text, {
    List<Attachment> attachments = const [],
    PromptHow how = PromptHow.queue,
  }) => {
    'Prompt': {
      'sid': sid,
      'text': text,
      'attachments': [for (final a in attachments) a.toJson()],
      'how': wireName(how),
    },
  };
  static Object interrupt(String sid) => {
    'Interrupt': {'sid': sid},
  };

  /// Stop right after sending: the newest prompt comes back (`Resp.takenBack`), or the turn is
  /// only interrupted (`Ok`).
  static Object takeBack(String sid) => {
    'TakeBack': {'sid': sid},
  };
  static Object background(String sid, {String? toolUseId}) => {
    'Background': {'sid': sid, 'tool_use_id': toolUseId},
  };
  static Object answerQuestion(String sid, String requestId, Answer answer) => {
    'AnswerQuestion': {
      'sid': sid,
      'request_id': requestId,
      'answer': answer.json,
    },
  };
  static Object takeover(String sid, Mode to) => {
    'Takeover': {'sid': sid, 'to': wireName(to)},
  };
  static Object setModel(String sid, String model) => {
    'SetModel': {'sid': sid, 'model': model},
  };
  static Object paneOpen(String sid) => {
    'PaneOpen': {'sid': sid},
  };

  /// Push through ntfy: the box posts to [topic] per [rules] (`PushRules.toJson`).
  static Object pushRegister(String topic, Object rules) => {
    'PushRegister': {
      'platform': 'Ntfy',
      'token': topic,
      'rules': rules is Map ? rules : (rules as dynamic).toJson(),
    },
  };
  static Object pushUnregister(String topic) => {
    'PushUnregister': {'token': topic},
  };
  static Object paneClose(PaneId pane) => {
    'PaneClose': {'pane': pane.toJson()},
  };
  static Object issueCreate(String repo, String title, String body) => {
    'IssueCreate': {'repo': repo, 'title': title, 'body': body},
  };
  static Object issuePatch(int id, int baseRev, List<Object> fields) => {
    'IssuePatch': {'id': id, 'base_rev': baseRev, 'fields': fields},
  };
  static Object issueDelete(int id) => {
    'IssueDelete': {'id': id},
  };
  static Object upload(String name, int len) => {
    'Upload': {'name': name, 'len': len},
  };
  static Object fsList(String sid, String path) => {
    'FsList': {'sid': sid, 'path': path},
  };
  static Object fsRead(String sid, String path) => {
    'FsRead': {'sid': sid, 'path': path},
  };
  static Object folders(String path, {bool make = false}) => {
    'Folders': {'path': path, 'make': make},
  };
  static Object boardApi(String method, String path, {String body = ''}) => {
    'BoardApi': {'method': method, 'path': path, 'body': body},
  };
  static Object blobPut(String name, List<int> data) => {
    'BlobPut': {'name': name, 'data': data},
  };
  static const Object caps = 'Caps';
  static Object setCap(int account, {int? fiveHour, int? weekly}) => {
    'SetCap': {'account': account, 'five_hour': fiveHour, 'weekly': weekly},
  };
  static const Object serverInfo = 'ServerInfo';
  static const Object commands = 'Commands';
  static Object cyclePermission(String sid) => {
    'CyclePermission': {'sid': sid},
  };
  static Object sessionLabel(String sid, String? label) => {
    'SessionLabel': {'sid': sid, 'label': label},
  };
  static Object show(String sid, String name, List<int> blob, String caption) =>
      {
        'Show': {'sid': sid, 'name': name, 'blob': blob, 'caption': caption},
      };
  static const Object prs = 'Prs';
  static Object prVideo(String name) => {
    'PrVideo': {'name': name},
  };
  static Object prMerge(int number) => {
    'PrMerge': {'number': number},
  };
  static Object prClose(int number) => {
    'PrClose': {'number': number},
  };
  static Object prFix(int number) => {
    'PrFix': {'number': number},
  };
  static Object prAutoFix({bool? on}) => {
    'PrAutoFix': {'on': on},
  };
  static Object resend(String sid, String itemId) => {
    'Resend': {'sid': sid, 'item_id': itemId},
  };
  static Object sessionOf(String agentSid) => {
    'SessionOf': {'agent_sid': agentSid},
  };
}

class Pr {
  const Pr({
    required this.number,
    required this.title,
    required this.branch,
    required this.author,
    required this.url,
    required this.body,
    required this.additions,
    required this.deletions,
    this.video,
    this.mergeable,
    this.fixing,
    this.merging = false,
  });
  final int number;
  final String title;
  final String branch;
  final String author;
  final String url;
  final String body;
  final int additions;
  final int deletions;
  final String? video;
  final bool? mergeable;
  final String? fixing;
  final bool merging;

  factory Pr.fromJson(Json j) => Pr(
    number: j['number'] as int,
    title: j['title'] as String,
    branch: j['branch'] as String,
    author: j['author'] as String,
    url: j['url'] as String,
    body: j['body'] as String? ?? '',
    additions: j['additions'] as int? ?? 0,
    deletions: j['deletions'] as int? ?? 0,
    video: j['video'] as String?,
    mergeable: j['mergeable'] as bool?,
    fixing: j['fixing'] as String?,
    merging: j['merging'] as bool? ?? false,
  );
}

class FsEntry {
  const FsEntry(this.name, this.dir, this.len);
  final String name;
  final bool dir;
  final int len;
  factory FsEntry.fromJson(Json j) =>
      FsEntry(j['name'] as String, j['dir'] as bool, j['len'] as int);
}

class AccountCap {
  const AccountCap(this.account, this.fiveHour, this.weekly);
  final int account;
  final int? fiveHour;
  final int? weekly;
  factory AccountCap.fromJson(Json j) => AccountCap(
    j['account'] as int,
    j['five_hour'] as int?,
    j['weekly'] as int?,
  );
}

/// `rpc::ErrCode` + message: what a failed request throws.
class FemmeError implements Exception {
  const FemmeError(this.code, this.msg);
  final String code;
  final String msg;
  factory FemmeError.fromJson(Json j) =>
      FemmeError(j['code'] as String? ?? 'Internal', j['msg'] as String? ?? '');
  @override
  String toString() => '$code: $msg';
}

/// A successful `rpc::Resp`: the variant name and its raw value, with typed accessors per variant.
class Resp {
  const Resp(this.variant, this.value);
  final String variant;
  final Object? value;

  factory Resp.fromJson(Object json) {
    final (tag, v) = tagOf(json);
    return Resp(tag, v);
  }

  Json get _m => value as Json;

  bool get isOk => variant == 'Ok';
  String get session => value as String;
  PaneId get pane => PaneId.fromJson(_m);
  ({List<Item> items, int? nextBefore}) get page => (
    items: listOf(_m['items'], Item.fromJson),
    nextBefore: _m['next_before'] as int?,
  );
  List<int> get blob => bytesFromJson(value);
  int get issue => value as int;
  int get uploadStream => value as int;
  List<FsEntry> get fsEntries => listOf(value, FsEntry.fromJson);
  List<int> get fsData => bytesFromJson(value);
  ({String path, List<String> names}) get folders =>
      (path: _m['path'] as String, names: (_m['names'] as List).cast<String>());
  ({int status, String body}) get boardApi =>
      (status: _m['status'] as int, body: _m['body'] as String);
  List<int> get blobHash => bytesFromJson(value);
  List<AccountCap> get caps => listOf(value, AccountCap.fromJson);
  ({String version, String? deployed}) get serverInfo =>
      (version: _m['version'] as String, deployed: _m['deployed'] as String?);
  List<String> get commands => (_m['names'] as List).cast<String>();
  List<Pr> get prs => listOf(value, Pr.fromJson);
  bool get prAutoFix => value as bool;

  /// The text of the prompt a `TakeBack` brought back; null when it only interrupted.
  String? get takenBack => variant == 'TakenBack' ? _m['text'] as String : null;

  @override
  String toString() => 'Resp.$variant';
}

/// `rpc::S2C`.
sealed class S2C {
  const S2C();

  factory S2C.fromJson(Object json) {
    final (tag, v) = tagOf(json);
    final m = v is Json ? v : const <String, dynamic>{};
    return switch (tag) {
      'Welcome' => Welcome(
        m['proto'] as int,
        m['server_version'] as String,
        (m['epoch'] as num).toInt(),
      ),
      'Resp' => RespMsg(m['op'] as String, m['result'] as Object),
      'Changes' => Changes(
        Scope.fromJson(m['scope'] as Object),
        m['rev'] as int,
        [for (final r in m['rows'] as List) Row.fromJson(r as Object)],
        reset: m['reset'] as bool? ?? false,
      ),
      'Delta' => Delta(
        m['sid'] as String,
        m['item_id'] as String,
        m['offset'] as int,
        m['text'] as String,
      ),
      'SnapshotOnBulk' => SnapshotOnBulk(
        Scope.fromJson(m['scope'] as Object),
        m['rev'] as int,
        m['stream'] as int,
      ),
      _ => throw FormatException('unknown S2C $tag'),
    };
  }
}

class Welcome extends S2C {
  const Welcome(this.proto, this.serverVersion, this.epoch);
  final int proto;
  final String serverVersion;
  final int epoch;
}

/// `S2C::Resp`. [result] is `{"Ok": resp}` or `{"Err": {code, msg}}`.
class RespMsg extends S2C {
  const RespMsg(this.op, this.result);
  final String op;
  final Object result;

  /// The response, or throws the [FemmeError] it carries.
  Resp unwrap() {
    final (tag, v) = tagOf(result);
    if (tag == 'Err') throw FemmeError.fromJson(v as Json);
    return Resp.fromJson(v as Object);
  }
}

class Changes extends S2C {
  const Changes(this.scope, this.rev, this.rows, {this.reset = false});
  final Scope scope;
  final int rev;
  final List<Row> rows;
  final bool reset;

  Object toJson() => {
    'Changes': {
      'scope': scope.toJson(),
      'rev': rev,
      'rows': [for (final r in rows) r.toJson()],
      'reset': reset,
    },
  };
}

/// Streaming text of an in-progress item. [offset] counts the characters already applied.
class Delta extends S2C {
  const Delta(this.sid, this.itemId, this.offset, this.text);
  final String sid;
  final String itemId;
  final int offset;
  final String text;
}

class SnapshotOnBulk extends S2C {
  const SnapshotOnBulk(this.scope, this.rev, this.stream);
  final Scope scope;
  final int rev;
  final int stream;
}

/// `C2S::Hello`.
Object hello(int clientInstance, Cursors cursors) => {
  'Hello': {
    'proto': protoVersion,
    'client_instance': clientInstance,
    'cursors': cursors.toJson(),
  },
};

/// `C2S::Req`.
Object reqMsg(String op, Object req) => {
  'Req': {'op': op, 'req': req},
};
