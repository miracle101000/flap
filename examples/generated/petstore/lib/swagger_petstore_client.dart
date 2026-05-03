import 'package:dio/dio.dart';

import 'pet.dart';
import 'pets.dart';
import 'error_model.dart';

class SwaggerPetstoreClient {
  SwaggerPetstoreClient({required String baseUrl})
      : _dio = Dio(BaseOptions(baseUrl: baseUrl));

  final Dio _dio;

  Future<Pets> listPets({int? limit,}) async {
    final queryParameters = <String, dynamic>{
      if (limit != null) 'limit': limit,
    };
    final response = await _dio.request<dynamic>(
      '/pets',
      options: Options(method: 'GET'),
      queryParameters: queryParameters,
    );
    final data = response.data;
    return (data as List<dynamic>)
        .map((e) => Pet.fromJson(e as Map<String, dynamic>))
        .toList();
  }

  Future<void> createPets({required Pet body,}) async {
    await _dio.request<dynamic>(
      '/pets',
      options: Options(method: 'POST'),
      data: body.toJson(),
    );
  }

  Future<Pet> showPetById({required String petId,}) async {
    final response = await _dio.request<dynamic>(
      '/pets/${petId}',
      options: Options(method: 'GET'),
    );
    final data = response.data;
    return Pet.fromJson(data as Map<String, dynamic>);
  }
}
