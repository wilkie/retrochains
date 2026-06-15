int int86(int, void *, void *);
int main(void) {
  return int86(0x10, 0, 0);
}
