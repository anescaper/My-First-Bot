FROM python:3.12-slim

WORKDIR /app

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

COPY strategy_service/ strategy_service/

EXPOSE 8100

CMD ["uvicorn", "strategy_service.main:app", "--host", "0.0.0.0", "--port", "8100", "--workers", "4"]
