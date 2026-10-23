import 'dart:io';

import 'package:flutter_test/flutter_test.dart';

/// iOS and tvOS link liberika statically and look its functions up in the
/// running executable (`dlsym` from the Swift plugin, `DynamicLibrary.process()`
/// from the Dart exports). The linker drops what nothing references, so every
/// function those lookups need is named in the podspec's `-u` list.
void main() {
  final header = File('native/include/erika.h').readAsStringSync();
  final declared = RegExp(r'\b(erika_\w+)\s*\(')
      .allMatches(header.replaceAll(RegExp(r'/\*.*?\*/', dotAll: true), ''))
      .map((match) => match.group(1)!)
      .toSet();

  for (final platform in <String>['ios', 'tvos']) {
    test('$platform force-links the export and diagnostics functions', () {
      final podspec = File('$platform/erika_flutter.podspec').readAsStringSync();
      final list = RegExp(r'erika_cabi_symbols = %w\[(.*?)\]', dotAll: true)
          .firstMatch(podspec)!
          .group(1)!
          .split(RegExp(r'\s+'))
          .where((name) => name.isNotEmpty)
          .toSet();
      for (final symbol in <String>[
        'erika_export_gif',
        'erika_export_frame',
        'erika_frame_export_result_free',
        'erika_frame_export_session_open',
        'erika_frame_export_session_frame',
        'erika_frame_export_session_close',
        'erika_last_error_message',
        'erika_string_free',
        'erika_presenter_invoke_json',
        'erika_presenter_get_diagnostics_json',
        'erika_log_read_json',
      ]) {
        expect(list, contains(symbol));
      }
      // A name that liberika does not define would fail the final link.
      expect(declared.containsAll(list), isTrue,
          reason: 'not in erika.h: ${list.difference(declared)}');
    });
  }
}
