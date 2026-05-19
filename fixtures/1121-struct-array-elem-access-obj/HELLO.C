struct S { int x; };
int main(void) {
  struct S arr[2];
  arr[0].x = 5;
  arr[1].x = 7;
  return arr[0].x + arr[1].x;
}
