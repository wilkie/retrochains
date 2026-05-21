struct R { int a; int b; int c; };
int main(void) {
  struct R arr[3];
  int i;
  int sum = 0;
  for (i = 0; i < 3; i++) {
    arr[i].a = i;
    arr[i].b = i + 10;
    arr[i].c = i + 100;
  }
  for (i = 0; i < 3; i++) {
    sum += arr[i].a + arr[i].b + arr[i].c;
  }
  return sum;
}
