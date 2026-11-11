import 'package:femme_mobile/net/models.dart';
import 'package:femme_mobile/ui/chat.dart';
import 'package:femme_mobile/ui/terminal.dart';
import 'package:femme_mobile/ui/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

import 'fixtures.dart';

Session session({
  String status = 'Working',
  String attention = 'None',
  String agent = 'Claude',
  String mode = 'Headless',
}) => Session.fromJson(
  sessionJson(status: status, attention: attention)
    ..['agent'] = agent
    ..['mode'] = mode,
);

void main() {
  test(
    'a shell or TUI session opens the terminal, a headless agent the chat',
    () {
      expect(sessionScreen(session(), sid), isA<ChatScreen>());
      expect(
        sessionScreen(null, sid),
        isA<ChatScreen>(),
        reason: 'not seen yet',
      );
      final shell = sessionScreen(session(agent: 'Shell'), sid);
      expect(shell, isA<TerminalScreen>());
      expect((shell as TerminalScreen).sid, sid);
      expect(sessionScreen(session(mode: 'Tui'), sid), isA<TerminalScreen>());
    },
  );
}
