import 'package:femme_mobile/net/models.dart';
import 'package:femme_mobile/ui/chat.dart';
import 'package:femme_mobile/ui/follow.dart';
import 'package:femme_mobile/ui/terminal.dart';
import 'package:femme_mobile/ui/widgets.dart';
import 'package:flutter/widgets.dart';
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
      // every session screen is wrapped in Following (it moves when the session closes)
      Widget screen(Session? s) => (sessionScreen(s, sid) as Following).child;
      expect(screen(session()), isA<ChatScreen>());
      expect(screen(null), isA<ChatScreen>(), reason: 'not seen yet');
      final shell = screen(session(agent: 'Shell'));
      expect(shell, isA<TerminalScreen>());
      expect((shell as TerminalScreen).sid, sid);
      expect(screen(session(mode: 'Tui')), isA<TerminalScreen>());
    },
  );
}
