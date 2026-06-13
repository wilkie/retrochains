void geninterrupt(int);
int main(void) {
  geninterrupt(0x21);
  return 0;
}
