int one(int x) { return x + 1; }
int two(int x) { return x + 2; }
int three(int x) { return x + 3; }
int main(void) {
  int (*ops[3])(int);
  int i;
  int sum;
  ops[0] = one;
  ops[1] = two;
  ops[2] = three;
  sum = 0;
  for (i = 0; i < 3; i = i + 1) {
    sum = sum + ops[i](10);
  }
  return sum;
}
