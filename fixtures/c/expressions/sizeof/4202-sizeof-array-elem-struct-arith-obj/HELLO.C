struct P { int x; char c; };
int main(void) {
  int a[5];
  struct P p;
  return sizeof a / sizeof a[0] + sizeof(struct P) - sizeof p.x;
}
