import 'package:flutter/widgets.dart';

import 'state/prefs.dart';
import 'ui/app.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  runApp(FemmeApp(prefs: await Prefs.load()));
}
