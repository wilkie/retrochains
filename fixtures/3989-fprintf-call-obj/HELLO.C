int fprintf(void *, char *, ...);
void *stdout;
int main(void) {
  fprintf(stdout, "hi\n");
  return 0;
}
