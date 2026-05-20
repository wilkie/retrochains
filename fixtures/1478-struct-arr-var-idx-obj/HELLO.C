struct S { int x; };
struct S arr[3];
int main(void) {
  int i = 1;
  arr[i].x = 99;
  return arr[i].x;
}
