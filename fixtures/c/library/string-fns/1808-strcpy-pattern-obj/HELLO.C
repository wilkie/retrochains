int my_copy(char *d, char *s) {
  int n = 0;
  while (*d++ = *s++) n++;
  return n;
}
int main(void) {
  char buf[8];
  return my_copy(buf, "HI");
}
