int sq(int x) { return x * x; }
int dbl(int x) { return x * 2; }
int main(void) {
  static int (*ops[2])(int) = {sq, dbl};
  int i;
  int total = 0;
  for (i = 0; i < 2; i++) total += ops[i](5);
  return total;
}
