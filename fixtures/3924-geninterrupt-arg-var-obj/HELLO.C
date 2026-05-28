void geninterrupt(int);
int main(void) {
  int n = 0x21;
  geninterrupt(n);
  return 0;
}
