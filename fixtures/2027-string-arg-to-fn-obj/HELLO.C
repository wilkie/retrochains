int len_to_end(char *s) {
  int n = 0;
  while (*s++) n++;
  return n;
}
int main(void) {
  return len_to_end("Hello");
}
