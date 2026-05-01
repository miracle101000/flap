import 'package:dio/dio.dart';

import 'pet.dart';

class SecurePetstoreClient {
  SecurePetstoreClient({
    required String baseUrl,
    String? apiKeyAuth,
    String? bearerAuth,
  }) : _dio = Dio(BaseOptions(baseUrl: baseUrl)) {
    _dio.interceptors.add(
      InterceptorsWrapper(
        onRequest: (options, handler) {
          if (apiKeyAuth != null) {
            options.headers['X-API-Key'] = apiKeyAuth;
          }
          if (bearerAuth != null) {
            options.headers['Authorization'] = 'Bearer $bearerAuth';
          }
          handler.next(options);
        },
      ),
    );
  }

  final Dio _dio;

  // GET /pets
  Future<Pet> listPets() async {
    final response = await _dio.request<dynamic>(
      '/pets',
      options: Options(method: 'GET'),
    );
    final data = response.data;
    return Pet.fromJson(data as Map<String, dynamic>);
  }
}
