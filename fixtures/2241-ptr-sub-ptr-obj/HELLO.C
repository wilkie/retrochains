int main(void) {
  static int arr[10];
  int *a = &arr[7];
  int *b = &arr[2];
  return (int)(a - b);
}
