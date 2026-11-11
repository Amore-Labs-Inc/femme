import 'package:femme_mobile/net/models.dart';
import 'package:femme_mobile/ui/theme.dart';
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
  test('status dot follows the desktop lanes', () {
    expect(statusColor(session(status: 'Working')), FemmeColors.work);
    expect(
      statusColor(session(status: 'Working', attention: 'Blocked')),
      FemmeColors.work,
      reason: 'working wins, as Lane::of matches it first',
    );
    expect(statusColor(session(status: 'Blocked')), FemmeColors.pause);
    expect(
      statusColor(session(status: 'Idle', attention: 'Blocked')),
      FemmeColors.pause,
    );
    expect(statusColor(session(status: 'Starting')), FemmeColors.queue);
    expect(statusColor(session(status: 'Idle')), FemmeColors.queue);
    for (final s in ['Interrupted', 'Paused', 'Crashed', 'Failed']) {
      expect(statusColor(session(status: s)), FemmeColors.bad, reason: s);
    }
    expect(statusColor(session(status: 'Closed')), FemmeColors.done);
  });
}
