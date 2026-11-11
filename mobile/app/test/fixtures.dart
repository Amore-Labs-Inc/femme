/// JSON as femmed's serde_json writes it, from the femme-proto struct definitions.
library;

const sid = '01JD3Q8Z9X4K2M7N5P6R8S0T1V';
const sid2 = '01JD3Q8Z9X4K2M7N5P6R8S0T2W';

Map<String, dynamic> sessionJson({
  String id = sid,
  int rev = 41,
  String status = 'Working',
  String attention = 'None',
  String? label = 'lovable-lesson',
  String? name,
  String cwd = '/home/cem/Documents/Web_Apps/Platform',
}) => {
  'id': id,
  'handle': 'k3',
  'rev': rev,
  'name': name,
  'owner': 'board',
  'agent': 'Claude',
  'mode': 'Headless',
  'cwd': cwd,
  'label': label,
  'agent_sid': 'c8eed47b-e7a7-4739-8260-f78e891051e8',
  'model': 'claude-opus-5-5',
  'status': status,
  'attention': attention,
  'created': 1794300000000,
  'closed': null,
  'permission': 'acceptEdits',
  'context': {'used': 81234, 'window': 200000, 'compactions': 1},
  'effort': 'high',
  'goal': {'condition': 'tests pass', 'state': 'NotMet', 'reason': '2 failing'},
};

Map<String, dynamic> itemJson({
  String sidV = sid,
  String id = 'msg_01',
  int seq = 10,
  int rev = 50,
  String kind = 'AssistantMessage',
  String status = 'Completed',
  String? body = '{"text":"Done: the build is green."}',
}) => {
  'sid': sidV,
  'item_id': id,
  'seq': seq,
  'rev': rev,
  'kind': kind,
  'status': status,
  'preview': 'Done: the build is green.',
  'body': body,
  'blob_hash': null,
  'ts': 1794300001000,
  'parent': null,
};

const questionBody =
    '{"request_id":"toolu_01ABC","prompt":"Fold it into one lesson?",'
    '"options":["yes, fold it","no, keep two"],"multi":false,"free_text":true,'
    '"answered":null}';

Map<String, dynamic> changes(
  Object scope,
  int rev,
  List<Object> rows, {
  bool reset = false,
}) => {
  'Changes': {'scope': scope, 'rev': rev, 'rows': rows, 'reset': reset},
};
