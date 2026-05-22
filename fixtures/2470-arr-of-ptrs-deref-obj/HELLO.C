int main(void) {
  int a;
  int b;
  int *arr[2];
  a = 11;
  b = 22;
  arr[0] = &a;
  arr[1] = &b;
  *arr[0] = 100;
  *arr[1] = 200;
  return a + b;
}
