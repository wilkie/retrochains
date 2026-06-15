int add5(int x) { return x + 5; }
int sub3(int x) { return x - 3; }
int main(void) {
  int (*ops[2])(int);
  ops[0] = add5;
  ops[1] = sub3;
  return (*ops[0])(10) + (*ops[1])(20);
}
